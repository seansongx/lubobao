// 同步录播宝的版本号：node scripts/set-version.mjs 0.2.0
// 改写 package.json、package-lock.json、src-tauri/tauri.conf.json、Cargo.toml 的 [workspace.package]
// 和 Cargo.lock 中的 lubobao、lubobao-core、lubobao-cli。改完可用 scripts/check-version.mjs 核对。
import { readFileSync, writeFileSync } from "node:fs";

const version = process.argv[2];
if (!/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(version ?? "")) {
  console.error("用法：node scripts/set-version.mjs <major.minor.patch>");
  process.exit(1);
}

function editJson(path, edit) {
  const data = JSON.parse(readFileSync(path, "utf8"));
  edit(data);
  writeFileSync(path, JSON.stringify(data, null, 2) + "\n");
}

function replaceOnce(path, pattern, replacement) {
  const text = readFileSync(path, "utf8");
  if (!pattern.test(text)) throw new Error(`${path} 中没有找到版本号`);
  writeFileSync(path, text.replace(pattern, replacement));
}

editJson("package.json", (d) => {
  d.version = version;
});
editJson("package-lock.json", (d) => {
  d.version = version;
  d.packages[""].version = version;
});
editJson("src-tauri/tauri.conf.json", (d) => {
  d.version = version;
});
replaceOnce(
  "Cargo.toml",
  /^(\[workspace\.package\][\s\S]*?\r?\nversion = )"[^"]*"/m,
  `$1"${version}"`,
);
for (const name of ["lubobao", "lubobao-core", "lubobao-cli"]) {
  replaceOnce(
    "Cargo.lock",
    new RegExp(`(\\r?\\nname = "${name}"\\r?\\nversion = )"[^"]*"`),
    `$1"${version}"`,
  );
}

console.log(`版本号已改为 ${version}`);
