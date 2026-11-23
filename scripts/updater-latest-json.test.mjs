#!/usr/bin/env node
// scripts/updater-latest-json.mjs 的本地检查：用假的更新包和 .sig 生成清单，核对结构；缺文件、缺签名、
// 正文没换占位句时必须失败。test.yml 里运行，本地也可以直接跑：
//   node --test scripts/updater-latest-json.test.mjs

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { after, test } from "node:test";
import { fileURLToPath } from "node:url";

const SCRIPT = fileURLToPath(new URL("./updater-latest-json.mjs", import.meta.url));
const VERSION = "1.2.3";
const FILES = {
  "windows-x86_64": `lubobao_${VERSION}_windows_x64-setup.exe`,
  "darwin-aarch64": `lubobao_${VERSION}_macos_arm64.app.tar.gz`,
  "darwin-x86_64": `lubobao_${VERSION}_macos_x64.app.tar.gz`,
  "linux-x86_64": `lubobao_${VERSION}_linux_amd64.AppImage`,
};
const BODY = `## 更新内容

- 修复了一个问题

<!-- 以上为应用内更新说明 -->

## 下载
`;

const dirs = [];
after(() => Promise.all(dirs.map((dir) => rm(dir, { recursive: true, force: true }))));

/** 一个装着全部更新包、.sig 和无关文件的临时目录；skip 里的文件不建。 */
async function fixture(skip = []) {
  const dir = await mkdtemp(path.join(tmpdir(), "lubobao-latest-json-"));
  dirs.push(dir);
  for (const [key, name] of Object.entries(FILES)) {
    if (!skip.includes(name)) await writeFile(path.join(dir, name), `package ${key}`);
    if (!skip.includes(`${name}.sig`)) await writeFile(path.join(dir, `${name}.sig`), `c2lnLSR7a2V5fQ==${key}\r\n`);
  }
  await writeFile(path.join(dir, `lubobao_${VERSION}_linux_amd64.deb`), "deb");
  await writeFile(path.join(dir, `lubobao_${VERSION}_macos_arm64.dmg`), "dmg");
  await writeFile(path.join(dir, "SHA256SUMS.txt"), "");
  return dir;
}

function run(...args) {
  const result = spawnSync(process.execPath, [SCRIPT, ...args], {
    encoding: "utf8",
    env: { ...process.env, GITHUB_REPOSITORY: "someone/lubobao-fork" },
  });
  return { code: result.status, output: `${result.stdout}${result.stderr}` };
}

test("generates a Tauri static manifest with every platform", async () => {
  const dir = await fixture();
  const result = run("--dir", dir, "--tag", `v${VERSION}`, "--pub-date", "2026-11-23T08:00:00Z");
  assert.equal(result.code, 0, result.output);
  const json = JSON.parse(await readFile(path.join(dir, "latest.json"), "utf8"));
  assert.deepEqual(Object.keys(json), ["version", "pub_date", "platforms"]);
  assert.equal(json.version, VERSION);
  assert.equal(json.pub_date, "2026-11-23T08:00:00Z");
  assert.deepEqual(Object.keys(json.platforms).sort(), Object.keys(FILES).sort());
  for (const [key, name] of Object.entries(FILES)) {
    assert.deepEqual(json.platforms[key], {
      signature: `c2lnLSR7a2V5fQ==${key}`,
      url: `https://github.com/someone/lubobao-fork/releases/download/v${VERSION}/${name}`,
    });
  }
});

test("takes owner and repo from --repo before GITHUB_REPOSITORY", async () => {
  const dir = await fixture();
  const out = path.join(dir, "out.json");
  assert.equal(run("--dir", dir, "--tag", `v${VERSION}`, "--repo", "a/b", "--out", out).code, 0);
  const json = JSON.parse(await readFile(out, "utf8"));
  assert.match(json.platforms["linux-x86_64"].url, /^https:\/\/github\.com\/a\/b\/releases\/download\//);
  assert.ok(!Number.isNaN(Date.parse(json.pub_date)));
});

test("fails without writing when a package or signature is missing", async () => {
  for (const missing of [FILES["darwin-x86_64"], `${FILES["windows-x86_64"]}.sig`]) {
    const dir = await fixture([missing]);
    const result = run("--dir", dir, "--tag", `v${VERSION}`);
    assert.notEqual(result.code, 0);
    assert.match(result.output, new RegExp(missing.replace(/[.]/g, "\\.")));
    await assert.rejects(readFile(path.join(dir, "latest.json")));
  }
});

test("fails for a version that does not match the file names", async () => {
  const dir = await fixture();
  assert.notEqual(run("--dir", dir, "--tag", "v9.9.9").code, 0);
  assert.notEqual(run("--dir", dir, "--tag", "latest").code, 0);
});

test("writes the release notes above the marker and keeps the platforms", async () => {
  const dir = await fixture();
  const file = path.join(dir, "latest.json");
  assert.equal(run("--dir", dir, "--tag", `v${VERSION}`).code, 0);
  const before = JSON.parse(await readFile(file, "utf8"));
  const body = path.join(dir, "body.md");
  await writeFile(body, BODY.replace(/\n/g, "\r\n"));
  const result = run("--set-notes", file, "--tag", `v${VERSION}`, "--release-body", body, "--pub-date", "2026-11-24T00:00:00Z");
  assert.equal(result.code, 0, result.output);
  const json = JSON.parse(await readFile(file, "utf8"));
  assert.deepEqual(Object.keys(json), ["version", "notes", "pub_date", "platforms"]);
  assert.equal(json.notes, "- 修复了一个问题");
  assert.equal(json.pub_date, "2026-11-24T00:00:00Z");
  assert.deepEqual(json.platforms, before.platforms);
});

test("refuses placeholder notes, a missing marker and another version", async () => {
  const dir = await fixture();
  const file = path.join(dir, "latest.json");
  assert.equal(run("--dir", dir, "--tag", `v${VERSION}`).code, 0);
  const body = path.join(dir, "body.md");
  await writeFile(body, "## 更新内容\n\n- （发布前把这里换成本版本的变化。）\n\n<!-- 以上为应用内更新说明 -->\n");
  assert.notEqual(run("--set-notes", file, "--tag", `v${VERSION}`, "--release-body", body).code, 0);
  await writeFile(body, "- 修复\n");
  assert.notEqual(run("--set-notes", file, "--tag", `v${VERSION}`, "--release-body", body).code, 0);
  await writeFile(body, BODY);
  assert.notEqual(run("--set-notes", file, "--tag", "v1.2.4", "--release-body", body).code, 0);
  assert.equal(JSON.parse(await readFile(file, "utf8")).notes, undefined);
});
