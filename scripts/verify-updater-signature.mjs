#!/usr/bin/env node
// 用 tauri.conf.json 里的更新公钥校验 Tauri 更新签名（minisign），与应用内 tauri-plugin-updater
// 的校验相同。release.yml 在收集安装包时对每个 .sig 运行一次：签名私钥和配置的公钥不是一对时
// 构建失败，不会发出一个装好的应用无法验证的版本。
//
//   node scripts/verify-updater-signature.mjs <文件> [<文件>.sig] [--pubkey <base64 公钥>] [--version <版本>]
//
// 公钥默认读 src-tauri/tauri.conf.json 的 plugins.updater.pubkey。给了 --version 时还检查签名的
// 可信注释里记录的版本（应用开启了 requireSignedVersion）。

import { createHash, createPublicKey, verify } from "node:crypto";
import { readFile } from "node:fs/promises";
import { parseArgs } from "node:util";

const { values: args, positionals } = parseArgs({
  allowPositionals: true,
  options: { pubkey: { type: "string" }, version: { type: "string" } },
});

function fail(message) {
  console.error(`✗ ${message}`);
  process.exit(1);
}

const [file, sigFile = `${file}.sig`] = positionals;
if (!file) fail("用法：verify-updater-signature.mjs <文件> [<文件>.sig] [--pubkey <base64>] [--version <版本>]");

const conf = JSON.parse(await readFile(new URL("../src-tauri/tauri.conf.json", import.meta.url), "utf8"));
const pubkeyBase64 = args.pubkey ?? conf.plugins?.updater?.pubkey;
if (!pubkeyBase64 || pubkeyBase64 === "LUBOBAO_UPDATER_PUBKEY_NOT_SET") fail("没有配置更新公钥");

/** minisign 文本（Tauri 再整体 base64 一次）里的各行。 */
function lines(base64, what) {
  const text = Buffer.from(base64.trim(), "base64").toString("utf8");
  const result = text.split(/\r?\n/).filter((line) => line.length > 0);
  if (result.length < 2) fail(`${what} 格式不对`);
  return result;
}

const keyBytes = Buffer.from(lines(pubkeyBase64, "公钥")[1], "base64");
if (keyBytes.length !== 42 || keyBytes.subarray(0, 2).toString() !== "Ed") fail("公钥不是 minisign Ed25519 公钥");
const keyId = keyBytes.subarray(2, 10);
// Ed25519 SubjectPublicKeyInfo 的固定前缀 + 32 字节公钥。
const publicKey = createPublicKey({
  key: Buffer.concat([Buffer.from("302a300506032b6570032100", "hex"), keyBytes.subarray(10)]),
  format: "der",
  type: "spki",
});

const sigLines = lines(await readFile(sigFile, "utf8"), "签名");
const sigBytes = Buffer.from(sigLines[1], "base64");
if (sigBytes.length !== 74) fail("签名长度不对");
const algorithm = sigBytes.subarray(0, 2).toString();
if (!keyId.equals(sigBytes.subarray(2, 10))) {
  fail(`签名的密钥 ID 与公钥不符：签名私钥和 tauri.conf.json 里的公钥不是一对（${sigFile}）`);
}
const data = await readFile(file);
// "ED"：先算 BLAKE2b-512 再签（Tauri 默认）；"Ed"：直接签原文。
const message = algorithm === "ED" ? createHash("blake2b512").update(data).digest() : data;
if (!verify(null, message, publicKey, sigBytes.subarray(10))) fail(`签名与文件内容不符：${file}`);

const trustedLine = sigLines.find((line) => line.startsWith("trusted comment: "));
const globalSig = sigLines[sigLines.indexOf(trustedLine) + 1];
if (!trustedLine || !globalSig) fail("签名缺少可信注释");
const trusted = trustedLine.slice("trusted comment: ".length);
const globalOk = verify(
  null,
  Buffer.concat([sigBytes.subarray(10), Buffer.from(trusted, "utf8")]),
  publicKey,
  Buffer.from(globalSig, "base64"),
);
if (!globalOk) fail("可信注释的签名不符");
if (args.version) {
  // tauri signer 写入 "version:<版本>"（与 tauri-plugin-updater 的 signed_version 解析一致）。
  const signed = /(?:^|\s)version:(\S+)/.exec(trusted)?.[1];
  if (signed !== args.version) fail(`签名记录的版本是 ${signed ?? "（无）"}，应为 ${args.version}`);
}
console.log(`✓ 签名有效：${file}（${trusted}）`);
