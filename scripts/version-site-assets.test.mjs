import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { versionAssets } from "./version-site-assets.mjs";

function site(files) {
  const dir = mkdtempSync(path.join(tmpdir(), "site-"));
  for (const [rel, body] of Object.entries(files)) {
    mkdirSync(path.dirname(path.join(dir, rel)), { recursive: true });
    writeFileSync(path.join(dir, rel), body);
  }
  return dir;
}

test("adds a content version to local href and src references", () => {
  const dir = site({ "styles.css": "a{}", "site.js": "1", "assets/x.webp": "img" });
  const { html } = versionAssets(
    '<link href="./styles.css"><script src="./site.js"></script><img src="./assets/x.webp">',
    dir,
  );
  assert.match(html, /href="\.\/styles\.css\?v=[0-9a-f]{10}"/);
  assert.match(html, /src="\.\/site\.js\?v=[0-9a-f]{10}"/);
  assert.match(html, /src="\.\/assets\/x\.webp\?v=[0-9a-f]{10}"/);
});

test("the version changes only when the file content changes", () => {
  const a = site({ "styles.css": "a{}" });
  const b = site({ "styles.css": "a{}" });
  const c = site({ "styles.css": "b{}" });
  const ref = '<link href="./styles.css">';
  assert.equal(versionAssets(ref, a).html, versionAssets(ref, b).html);
  assert.notEqual(versionAssets(ref, a).html, versionAssets(ref, c).html);
});

test("leaves missing files, external links and existing queries alone", () => {
  const dir = site({ "styles.css": "a{}" });
  const input =
    '<img src="./assets/missing.webp"><a href="https://example.com/x.css">x</a>' +
    '<link href="./styles.css?v=old"><a href="#download">d</a>';
  assert.equal(versionAssets(input, dir).html, input);
});

test("the same file referenced twice gets the same version", () => {
  const dir = site({ "assets/r.webp": "img" });
  const { html, versions } = versionAssets('<link href="./assets/r.webp"><img src="./assets/r.webp">', dir);
  assert.equal(versions.size, 1);
  const [v] = versions.values();
  assert.equal(html.split(`?v=${v}`).length, 3);
});
