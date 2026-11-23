#!/usr/bin/env node
// 部署官网前给 index.html 里引用的本站文件加上内容版本号，例如
//   ./styles.css  ->  ./styles.css?v=1a2b3c4d5e
// nginx 给 CSS、JS 和图片设了缓存（1 小时到 1 天），文件名不变时，老访客在缓存过期前会拿到
// 新 HTML 配旧样式。版本号取文件内容的 SHA-256 前 10 位，只有文件变了地址才变，没变的继续用缓存。
//
// 只处理 index.html 里 href="./…" 和 src="./…" 形式的引用；文件不存在（例如 HTML 注释里的示例）
// 或已经带 ? / # 的保持不变。直接改写传入目录里的 index.html，由 deploy-site.yml 在 rsync 前调用，
// 仓库里的文件不变。
//
//   node scripts/version-site-assets.mjs website

import { createHash } from "node:crypto";
import { existsSync, readFileSync, statSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const REF = /\b(href|src)="\.\/([^"?#]+)"/g;

export function versionAssets(html, siteDir) {
  const versions = new Map();
  const out = html.replace(REF, (whole, attr, rel) => {
    const file = path.join(siteDir, rel);
    if (!existsSync(file) || !statSync(file).isFile()) return whole;
    let v = versions.get(rel);
    if (!v) {
      v = createHash("sha256").update(readFileSync(file)).digest("hex").slice(0, 10);
      versions.set(rel, v);
    }
    return `${attr}="./${rel}?v=${v}"`;
  });
  return { html: out, versions };
}

function main(argv) {
  const siteDir = argv[0];
  if (!siteDir) {
    console.error("用法：node scripts/version-site-assets.mjs <网站目录>");
    process.exit(2);
  }
  const indexPath = path.join(siteDir, "index.html");
  const { html, versions } = versionAssets(readFileSync(indexPath, "utf8"), siteDir);
  writeFileSync(indexPath, html);
  for (const [rel, v] of versions) console.log(`${rel}?v=${v}`);
  if (versions.size === 0) {
    console.error("index.html 里没有找到可以加版本号的本站文件");
    process.exit(1);
  }
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main(process.argv.slice(2));
}
