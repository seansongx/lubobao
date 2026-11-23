// 发布前检查版本号是否一致：node scripts/check-version.mjs [v<x.y.z>]
// 比较 Cargo workspace（[workspace.package]）、src-tauri/tauri.conf.json、package.json、package-lock.json 和
// Cargo.lock 里录播宝各 crate 的版本；给出标签时还要求标签为 v<版本>。
import { readFileSync } from "node:fs";

const tag = process.argv[2];

function workspaceVersion() {
  const text = readFileSync("Cargo.toml", "utf8");
  const match = text.match(/^\[workspace\.package\][\s\S]*?\r?\nversion = "([^"]*)"/m);
  if (!match) throw new Error("Cargo.toml 的 [workspace.package] 中没有找到 version");
  return match[1];
}

function lockVersions() {
  const text = readFileSync("Cargo.lock", "utf8");
  const found = {};
  for (const name of ["lubobao", "lubobao-core", "lubobao-cli"]) {
    const match = text.match(new RegExp(`\r?\nname = "${name}"\r?\nversion = "([^"]*)"`));
    found[`Cargo.lock ${name}`] = match ? match[1] : "（缺失）";
  }
  return found;
}

function lockfileVersions() {
  const lock = JSON.parse(readFileSync("package-lock.json", "utf8"));
  return {
    "package-lock.json version": lock.version,
    'package-lock.json packages[""].version': lock.packages?.[""]?.version ?? "（缺失）",
  };
}

const versions = {
  "Cargo.toml [workspace.package]": workspaceVersion(),
  "src-tauri/tauri.conf.json": JSON.parse(readFileSync("src-tauri/tauri.conf.json", "utf8")).version,
  "package.json": JSON.parse(readFileSync("package.json", "utf8")).version,
  ...lockfileVersions(),
  ...lockVersions(),
};

const expected = tag ? tag.replace(/^v/, "") : versions["Cargo.toml [workspace.package]"];
let ok = true;
for (const [source, version] of Object.entries(versions)) {
  if (version !== expected) {
    console.error(`::error::${source} 的版本 ${version} 与 ${tag ? `标签 ${tag}` : "workspace 版本"} 不一致`);
    ok = false;
  }
}
if (tag && !/^v\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(tag)) {
  console.error(`::error::标签 ${tag} 不是 v<major.minor.patch>`);
  ok = false;
}
if (!ok) process.exit(1);
console.log(`版本号一致：${expected}`);
