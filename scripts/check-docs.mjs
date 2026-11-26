#!/usr/bin/env node
// project-docs checker
//
// 用法：
//   1. Hook 模式（无参数）：从 stdin 读取 Claude Code PreToolUse 的 JSON。
//      要在允许位置之外新建 .md 时 exit 2，把原因写到 stderr，Claude 会看到并改写到正确位置。
//      只在已初始化的项目（存在 .claude/project-docs.json）里生效；修改已有文件不拦截。
//   2. node check-docs.mjs --check   检查：位置、spec 元信息、决策编号、死链、超期计划、索引。
//                                    有错误时 exit 1，可用于 CI / 提交前检查。
//   3. node check-docs.mjs --index   重新生成 docs/README.md 里的索引。
//   4. node check-docs.mjs --audit   盘点项目里所有 .md（位置是否合规、行数、最后改动），再跑一次检查。
//   5. node check-docs.mjs --init    初始化：建目录、写配置、往 CLAUDE.md 写入规则、复制本脚本到 scripts/。
//
// 项目配置：<项目根>/.claude/project-docs.json
//   {
//     "disabled": false,        // true 时完全关闭
//     "docsDir": "docs",        // 文档目录
//     "allow": [],              // 额外允许放 .md 的位置（glob，相对项目根），如 "content/**"
//     "ignore": [],             // 完全不检查的位置（glob），如 "server/.github/**"
//     "planStaleDays": 30       // 计划超过多少天没动就提醒
//   }

import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';

const CONFIG_FILE = '.claude/project-docs.json';
const SPEC_STATUSES = ['planned', 'shipped', 'deprecated'];
const INDEX_START = '<!-- project-docs:index:start -->';
const INDEX_END = '<!-- project-docs:index:end -->';
const RULES_START = '<!-- project-docs:rules:start -->';
const RULES_END = '<!-- project-docs:rules:end -->';

const SKIP_DIRS = new Set([
  'node_modules', 'vendor', '.git', 'dist', 'build', 'out', 'release', 'target',
  '.next', '.nuxt', '.output', '.svelte-kit', '.astro', '.vercel', '.turbo',
  'coverage', '.cache', '.venv', 'venv', '__pycache__',
]);

// docs/ 之外默认允许的位置：工具按固定路径读取的文件
const DEFAULT_ALLOW = [
  '**/README.md', '**/CHANGELOG.md', '**/CLAUDE.md', '**/AGENTS.md',
  'LICENSE.md', 'CONTRIBUTING.md', 'SECURITY.md', 'CODE_OF_CONDUCT.md',
  '.claude/**', '.github/**',
];

const RULES_BLOCK = `${RULES_START}
## 文档

文档索引在 \`docs/README.md\`。写或改文档前先读它。

- 文档只能放在 \`docs/specs/\`（功能与需求）、\`docs/plans/\`（进行中的计划）、\`docs/decisions/\`（决策记录）和 \`docs/architecture.md\`，其他位置不新建 \`.md\`。
- 写之前先搜 \`docs/\`，已有就原地更新；一个事实只写一处，别处用链接。
- 需求和功能是同一个文件：需求阶段写进 \`docs/specs/\` 并标 \`status: planned\`，做完改成 \`shipped\` 并把内容改成实际行为。
- 不写工作总结、改动说明、分析报告：这些写进 commit message 或直接在回复里说。
- 计划写在 \`docs/plans/\`，完成后把结论并入 spec 或架构文档，然后删除计划文件。
- 改代码让文档失效时，同一次提交里改文档。
- 代码里能看出来的（参数、字段、目录结构）不写进文档。
- Bug 和任务提 issue，不写进文档。
- 改完文档运行 \`node scripts/check-docs.mjs --index\` 更新索引，再运行 \`node scripts/check-docs.mjs --check\`。
${RULES_END}`;

// ---- 基础工具 ---------------------------------------------------------------

const posix = (p) => p.split(path.sep).join('/');

function globToRegExp(glob) {
  let re = '';
  for (let i = 0; i < glob.length; i++) {
    const c = glob[i];
    if (c === '*') {
      if (glob[i + 1] === '*') {
        if (glob[i + 2] === '/') { re += '(?:.*/)?'; i += 2; }
        else { re += '.*'; i += 1; }
      } else re += '[^/]*';
    } else if (c === '?') re += '[^/]';
    else re += c.replace(/[.+^${}()|[\]\\]/g, '\\$&');
  }
  return new RegExp(`^${re}$`);
}

const matchAny = (rel, globs) => globs.some((g) => globToRegExp(g).test(rel));

function findUp(start, marker) {
  let dir = path.resolve(start);
  for (;;) {
    if (fs.existsSync(path.join(dir, marker))) return dir;
    const parent = path.dirname(dir);
    if (parent === dir) return null;
    dir = parent;
  }
}

function loadConfig(root) {
  const file = path.join(root, CONFIG_FILE);
  let raw = {};
  let exists = false;
  if (fs.existsSync(file)) {
    exists = true;
    try {
      raw = JSON.parse(fs.readFileSync(file, 'utf8'));
    } catch (e) {
      console.error(`[project-docs] ${CONFIG_FILE} 不是合法的 JSON：${e.message}`);
      process.exit(1);
    }
  }
  return {
    exists,
    disabled: raw.disabled === true,
    docsDir: posix(raw.docsDir || 'docs').replace(/\/+$/, ''),
    allow: Array.isArray(raw.allow) ? raw.allow : [],
    ignore: Array.isArray(raw.ignore) ? raw.ignore : [],
    planStaleDays: Number.isFinite(raw.planStaleDays) ? raw.planStaleDays : 30,
  };
}

const inSkippedDir = (rel) => rel.split('/').slice(0, -1).some((seg) => SKIP_DIRS.has(seg));

// 判断一个 .md 的位置是否合规。返回 null 表示合规，否则返回原因。
function placementProblem(rel, config) {
  if (matchAny(rel, config.allow)) return null;
  const d = config.docsDir;
  if (rel === d || rel.startsWith(`${d}/`)) {
    const sub = rel.slice(d.length + 1);
    if (sub === 'README.md' || sub === 'architecture.md') return null;
    if (/^specs\/.+\.md$/.test(sub)) return null;
    if (/^(plans|issues)\/[^/]+\.md$/.test(sub)) return null;
    if (/^decisions\/[^/]+\.md$/.test(sub)) {
      return /^decisions\/\d{4}-.+\.md$/.test(sub)
        ? null
        : '决策记录的文件名要写成 NNNN-标题.md，编号接着已有的最大编号';
    }
    return `${d}/ 下只有 README.md、architecture.md、specs/、plans/、decisions/ 这几个位置`;
  }
  if (matchAny(rel, DEFAULT_ALLOW)) return null;
  return `文档要放在 ${d}/ 里`;
}

function git(root, args) {
  return execFileSync('git', ['-c', 'core.quotepath=false', ...args], {
    cwd: root, encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'],
  });
}

function listMarkdown(root, config) {
  let files;
  try {
    files = git(root, ['ls-files', '-co', '--exclude-standard', '--', '*.md'])
      .split('\n').filter(Boolean);
  } catch {
    files = [];
    const walk = (dir) => {
      for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
        if (e.isDirectory()) {
          if (!SKIP_DIRS.has(e.name)) walk(path.join(dir, e.name));
        } else if (e.name.toLowerCase().endsWith('.md')) {
          files.push(posix(path.relative(root, path.join(dir, e.name))));
        }
      }
    };
    walk(root);
  }
  return files
    .filter((f) => fs.existsSync(path.join(root, f)))
    .filter((f) => !inSkippedDir(f) && !matchAny(f, config.ignore))
    .sort();
}

const read = (root, rel) => fs.readFileSync(path.join(root, rel), 'utf8').replace(/\r\n/g, '\n');

function parseDoc(text, rel) {
  const fm = text.match(/^---\n([\s\S]*?)\n---\n?/);
  const fields = {};
  if (fm) {
    for (const line of fm[1].split('\n')) {
      const i = line.indexOf(':');
      if (i > 0) fields[line.slice(0, i).trim()] = line.slice(i + 1).replace(/\s+#.*$/, '').trim();
    }
  }
  const body = fm ? text.slice(fm[0].length) : text;
  const h1 = body.match(/^#\s+(.+)$/m);
  return {
    hasFrontMatter: Boolean(fm),
    fields,
    title: (h1 ? h1[1] : path.basename(rel, '.md')).trim(),
  };
}

// 最后改动时间：已提交且没有本地修改的用提交时间，否则用文件修改时间
function lastChanged(root, rel) {
  try {
    if (!git(root, ['status', '--porcelain', '--', rel]).trim()) {
      const out = git(root, ['log', '-1', '--format=%ct', '--', rel]).trim();
      if (out) return Number(out) * 1000;
    }
  } catch {}
  return fs.statSync(path.join(root, rel)).mtimeMs;
}

// ---- 索引 -------------------------------------------------------------------

const cell = (s) => String(s || '').replace(/\|/g, '\\|');
const href = (p) => p.replace(/ /g, '%20').replace(/\(/g, '%28').replace(/\)/g, '%29');

function buildIndex(root, config, files) {
  const d = config.docsDir;
  const under = (dir) => files.filter((f) => f.startsWith(`${d}/${dir}/`));
  const info = (f) => ({ link: href(f.slice(d.length + 1)), ...parseDoc(read(root, f), f) });
  const lines = [INDEX_START, '<!-- 由 check-docs.mjs --index 生成，不要手改 -->', ''];

  lines.push('## 功能与需求（specs）', '');
  const specs = under('specs').map(info);
  if (specs.length) {
    lines.push('| 文档 | 状态 | 范围 |', '| --- | --- | --- |');
    for (const s of specs) lines.push(`| [${cell(s.title)}](${s.link}) | ${cell(s.fields.status)} | ${cell(s.fields.scope)} |`);
  } else lines.push('暂无。');

  lines.push('', '## 进行中的计划（plans）', '');
  const plans = under('plans').map(info);
  if (plans.length) for (const p of plans) lines.push(`- [${p.title}](${p.link})`);
  else lines.push('暂无。');

  lines.push('', '## 决策记录（decisions）', '');
  const decisions = under('decisions').map(info);
  if (decisions.length) for (const p of decisions) lines.push(`- [${p.title}](${p.link})`);
  else lines.push('暂无。');

  const others = [];
  if (files.includes(`${d}/architecture.md`)) others.push('- [架构](architecture.md)');
  for (const p of under('issues').map(info)) others.push(`- [${p.title}](${p.link})`);
  if (others.length) lines.push('', '## 其他', '', ...others);

  lines.push('', INDEX_END);
  return lines.join('\n');
}

function currentIndex(text) {
  const a = text.indexOf(INDEX_START);
  const b = text.indexOf(INDEX_END);
  return a >= 0 && b > a ? text.slice(a, b + INDEX_END.length) : null;
}

function writeIndex(root, config) {
  const d = config.docsDir;
  const file = path.join(root, d, 'README.md');
  const block = buildIndex(root, config, listMarkdown(root, config));
  let text;
  if (fs.existsSync(file)) {
    text = fs.readFileSync(file, 'utf8').replace(/\r\n/g, '\n');
    const old = currentIndex(text);
    text = old ? text.replace(old, () => block) : `${text.trimEnd()}\n\n${block}\n`;
  } else {
    fs.mkdirSync(path.dirname(file), { recursive: true });
    text = `# 文档索引\n\n项目文档的唯一入口。写或改文档前先在这里找有没有已有的。\n\n${block}\n`;
  }
  fs.writeFileSync(file, text);
  return posix(path.relative(root, file));
}

// ---- 检查 -------------------------------------------------------------------

function check(root, config) {
  const errors = [];
  const warnings = [];
  const d = config.docsDir;
  const files = listMarkdown(root, config);

  // 1. 位置
  for (const f of files) {
    const problem = placementProblem(f, config);
    if (problem) errors.push(`${f}：位置不对，${problem}`);
  }

  // 2. spec 元信息
  for (const f of files.filter((x) => x.startsWith(`${d}/specs/`))) {
    const doc = parseDoc(read(root, f), f);
    if (!doc.hasFrontMatter) { errors.push(`${f}：缺少元信息（status、scope）`); continue; }
    if (!SPEC_STATUSES.includes(doc.fields.status)) errors.push(`${f}：status 只能是 ${SPEC_STATUSES.join(' / ')}`);
    if (!doc.fields.scope) errors.push(`${f}：缺少 scope（一句话说明这个文件管什么、不管什么）`);
  }

  // 3. 决策编号不重复
  const seen = new Map();
  for (const f of files.filter((x) => x.startsWith(`${d}/decisions/`))) {
    const n = path.basename(f).match(/^(\d{4})-/)?.[1];
    if (!n) continue;
    if (seen.has(n)) errors.push(`${f}：编号 ${n} 和 ${seen.get(n)} 重复`);
    else seen.set(n, f);
  }

  // 4. 死链：docs/ 下的文件和根目录的 .md
  for (const f of files.filter((x) => x.startsWith(`${d}/`) || !x.includes('/'))) {
    const text = read(root, f).replace(/```[\s\S]*?```/g, '').replace(/`[^`\n]*`/g, '');
    for (const m of text.matchAll(/\]\(<?([^)\s>]+)>?(?:\s+"[^"]*")?\)/g)) {
      const link = m[1];
      if (/^([a-z][a-z0-9+.-]*:|#|\/)/i.test(link)) continue;
      let target = link.split('#')[0].split('?')[0];
      try { target = decodeURIComponent(target); } catch {}
      if (target && !fs.existsSync(path.join(root, path.dirname(f), target))) errors.push(`${f}：链接失效 ${link}`);
    }
  }

  // 5. 超期计划
  for (const f of files.filter((x) => x.startsWith(`${d}/plans/`))) {
    const days = Math.floor((Date.now() - lastChanged(root, f)) / 86_400_000);
    if (days > config.planStaleDays) {
      warnings.push(`${f}：${days} 天没动。做完了就把结论并入 spec 后删除，不做了就直接删除`);
    }
  }

  // 6. 索引是最新的
  const indexFile = `${d}/README.md`;
  if (fs.existsSync(path.join(root, indexFile))) {
    const old = currentIndex(read(root, indexFile));
    if (old === null) {
      if (config.exists) errors.push(`${indexFile}：没有索引区块，运行 check-docs.mjs --index 生成`);
    } else if (old !== buildIndex(root, config, files)) {
      errors.push(`${indexFile}：索引过期，运行 check-docs.mjs --index 更新`);
    }
  } else if (config.exists) {
    errors.push(`${indexFile}：不存在，运行 check-docs.mjs --index 生成`);
  }

  return { errors, warnings, files };
}

function report({ errors, warnings, files }) {
  for (const w of warnings) console.log(`提醒  ${w}`);
  for (const e of errors) console.log(`错误  ${e}`);
  console.log(`检查了 ${files.length} 个 Markdown 文件：${errors.length} 个错误，${warnings.length} 条提醒`);
}

// ---- 盘点 -------------------------------------------------------------------

function audit(root, config) {
  const files = listMarkdown(root, config);
  const withDates = files.length <= 300;
  console.log(`项目里的 Markdown 文件（${files.length} 个）：\n`);
  console.log('位置  行数   最后改动    路径');
  for (const f of files) {
    const ok = placementProblem(f, config) === null;
    const lines = read(root, f).split('\n').length;
    const date = withDates ? new Date(lastChanged(root, f)).toISOString().slice(0, 10) : '-';
    console.log(`${ok ? '合规' : '散落'}  ${String(lines).padStart(5)}  ${date}  ${f}`);
  }
  console.log('');
  report(check(root, config));
}

// ---- 初始化 -----------------------------------------------------------------

function init(root) {
  const done = [];
  const configPath = path.join(root, CONFIG_FILE);
  if (!fs.existsSync(configPath)) {
    fs.mkdirSync(path.dirname(configPath), { recursive: true });
    fs.writeFileSync(configPath, `${JSON.stringify({ allow: [], ignore: [], planStaleDays: 30 }, null, 2)}\n`);
    done.push(`新建 ${CONFIG_FILE}`);
  }
  const config = loadConfig(root);

  for (const dir of ['specs', 'plans', 'decisions']) {
    const full = path.join(root, config.docsDir, dir);
    if (!fs.existsSync(full)) {
      fs.mkdirSync(full, { recursive: true });
      fs.writeFileSync(path.join(full, '.gitkeep'), '');
      done.push(`新建 ${config.docsDir}/${dir}/`);
    }
  }

  // 规则写进 CLAUDE.md；只有 AGENTS.md 的项目写进 AGENTS.md
  const rulesName = !fs.existsSync(path.join(root, 'CLAUDE.md')) && fs.existsSync(path.join(root, 'AGENTS.md'))
    ? 'AGENTS.md' : 'CLAUDE.md';
  const rulesPath = path.join(root, rulesName);
  const rules = RULES_BLOCK.replaceAll('docs/', `${config.docsDir}/`);
  let text = fs.existsSync(rulesPath) ? fs.readFileSync(rulesPath, 'utf8') : '';
  const a = text.indexOf(RULES_START);
  const b = text.indexOf(RULES_END);
  if (a >= 0 && b > a) text = text.slice(0, a) + rules + text.slice(b + RULES_END.length);
  else text = text.trim() ? `${text.trimEnd()}\n\n${rules}\n` : `${rules}\n`;
  fs.writeFileSync(rulesPath, text);
  done.push(`写入文档规则到 ${rulesName}`);

  // 复制检查脚本，让 CI 和没装插件的人也能运行
  const self = import.meta.filename;
  const target = path.join(root, 'scripts', 'check-docs.mjs');
  if (path.resolve(self) !== path.resolve(target)) {
    const ours = !fs.existsSync(target) || fs.readFileSync(target, 'utf8').includes('// project-docs checker');
    if (ours) {
      fs.mkdirSync(path.dirname(target), { recursive: true });
      fs.copyFileSync(self, target);
      done.push('复制检查脚本到 scripts/check-docs.mjs');
    } else {
      done.push('scripts/check-docs.mjs 已存在且不是本插件的脚本，没有覆盖：请改名或删除后重新运行');
    }
  }

  done.push(`生成索引 ${writeIndex(root, config)}`);
  for (const line of done) console.log(`- ${line}`);
}

// ---- Hook -------------------------------------------------------------------

async function hook() {
  const chunks = [];
  for await (const c of process.stdin) chunks.push(c);
  let input;
  try { input = JSON.parse(Buffer.concat(chunks).toString('utf8')); } catch { process.exit(0); }
  if (!/^(Write|Edit|MultiEdit)$/.test(input.tool_name || '')) process.exit(0);
  let file = input.tool_input?.file_path;
  if (!file || !/\.md$/i.test(file)) process.exit(0);
  file = path.resolve(input.cwd || process.cwd(), file);
  if (fs.existsSync(file)) process.exit(0); // 修改已有文件不拦截

  const root = findUp(path.dirname(file), CONFIG_FILE);
  if (!root) process.exit(0); // 项目没有启用，或文件在项目之外
  const config = loadConfig(root);
  if (config.disabled) process.exit(0);
  const rel = posix(path.relative(root, file));
  if (inSkippedDir(rel) || matchAny(rel, config.ignore)) process.exit(0);
  const problem = placementProblem(rel, config);
  if (!problem) process.exit(0);

  const d = config.docsDir;
  process.stderr.write(`[project-docs] 不能在这里新建文档：${rel}
原因：${problem}。

先在 ${d}/ 里搜一下有没有已有的文档可以更新。确实要新建时，只能放在：
  ${d}/specs/<功能>.md           功能与需求，一个功能一个文件
  ${d}/plans/<主题>.md           进行中的计划，做完并入 spec 后删除
  ${d}/decisions/NNNN-<标题>.md  决策记录，写完不改
  ${d}/architecture.md           架构

如果这是工作总结、改动说明或分析报告：不要写成文件，直接在回复里说，或写进 commit message。
如果是 Bug 或任务：提 issue，不写成文档。
确实需要放在这个位置时，不要自己改配置绕过：请用户把路径加进 ${CONFIG_FILE} 的 allow。
`);
  process.exit(2);
}

// ---- 入口 -------------------------------------------------------------------

const mode = process.argv[2];
if (!mode) {
  await hook();
} else {
  const cwd = process.cwd();
  const root = findUp(cwd, CONFIG_FILE) || findUp(cwd, '.git') || cwd;
  if (mode === '--init') {
    init(findUp(cwd, '.git') || cwd);
  } else {
    const config = loadConfig(root);
    if (mode === '--check') {
      const result = check(root, config);
      report(result);
      process.exit(result.errors.length ? 1 : 0);
    } else if (mode === '--index') {
      console.log(`已更新 ${writeIndex(root, config)}`);
    } else if (mode === '--audit') {
      audit(root, config);
    } else {
      console.error('用法：check-docs.mjs [--check | --index | --audit | --init]');
      process.exit(1);
    }
  }
}
