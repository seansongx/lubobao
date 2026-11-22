// 构建 Docker 镜像时下载 FFmpeg：按 `@seankit/ffmpeg` 清单（与应用内置的 manifest.json 相同）
// 取 Linux 平台包，依次尝试各个地址（npmmirror 优先），校验 tarball 与可执行文件的 SHA-256 后
// 放到输出目录。
//
// 用法：node fetch-ffmpeg.mjs <manifest.json> <amd64|arm64> <输出目录>
import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { chmodSync, copyFileSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const [manifestPath, arch, outDir] = process.argv.slice(2);
if (!manifestPath || !arch || !outDir) {
  console.error("用法：node fetch-ffmpeg.mjs <manifest.json> <amd64|arm64> <输出目录>");
  process.exit(2);
}

const keys = { amd64: "linux-x64", arm64: "linux-arm64" };
const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
const key = manifest.aliases?.[keys[arch]] ?? keys[arch];
const pkg = key && manifest.platforms[key];
if (!pkg) {
  console.error(`清单里没有 ${arch} 的 FFmpeg 平台包`);
  process.exit(1);
}

const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");

async function download(url) {
  const response = await fetch(url, { signal: AbortSignal.timeout(10 * 60 * 1000) });
  if (!response.ok) throw new Error(`HTTP ${response.status}`);
  const bytes = Buffer.from(await response.arrayBuffer());
  if (bytes.length !== pkg.size) throw new Error(`大小不符：${bytes.length}，应为 ${pkg.size}`);
  const digest = sha256(bytes);
  if (digest !== pkg.sha256) throw new Error(`SHA-256 不符：${digest}`);
  return bytes;
}

let tarball;
for (const url of pkg.urls) {
  try {
    console.log(`下载 ${url}`);
    tarball = await download(url);
    break;
  } catch (error) {
    console.error(`  失败：${error.message}`);
  }
}
if (!tarball) {
  console.error(`所有地址都下载失败：${pkg.file}`);
  process.exit(1);
}

const work = mkdtempSync(join(tmpdir(), "ffmpeg-"));
const archive = join(work, pkg.file);
writeFileSync(archive, tarball);
const member = `package/${pkg.binary}`;
// 相对路径、在临时目录里解压：Windows 上的 GNU tar 会把 `C:` 当成远程主机。
// 平台包里的 LICENSE（GPL 全文）和 README.md（上游构建与源码位置）随可执行文件一起放进镜像。
const notices = ["package/LICENSE", "package/README.md"];
execFileSync("tar", ["-xzf", pkg.file, member, ...notices], { cwd: work, stdio: "inherit" });
const binary = join(work, member);
const binaryDigest = sha256(readFileSync(binary));
if (binaryDigest !== pkg.binarySha256) {
  console.error(`可执行文件 SHA-256 不符：${binaryDigest}，应为 ${pkg.binarySha256}`);
  process.exit(1);
}

mkdirSync(outDir, { recursive: true });
const target = join(outDir, "ffmpeg");
copyFileSync(binary, target);
chmodSync(target, 0o755);
for (const notice of notices) {
  copyFileSync(join(work, notice), join(outDir, notice.replace("package/", "")));
}
console.log(`FFmpeg ${manifest.ffmpegVersion}（${pkg.package}@${pkg.version}）已校验：${target}`);
