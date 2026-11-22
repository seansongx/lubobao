// 浏览器假数据（npm run dev，不在 Tauri 里时使用），方便调界面。
// 任务覆盖七种显示状态；FFmpeg 状态可用地址参数切换：
//   ?ffmpeg=checking | notInstalled | waiting | downloading | verifying | ready | updating | outdated | failed | fatal
// 录像库用 ?library=missing 模拟根目录不存在；把设置 live_save_path 改成其他路径也视为不存在。
// 动态有三天的历史记录，控制台可调用 window.__lubobaoMock.pushActivity("failure") 模拟新动态。
// 缺省时模拟一次下载：下载中 → 校验 → 可用。控制台可调用 window.__lubobaoMock.setFfmpeg("failed")。
// 显示状态的推导复刻 src-tauri/src/tasks/model.rs 的 derive_display，只用于假数据。

import type { UnlistenFn } from "@tauri-apps/api/event";

import type {
  ActivityEntry,
  ActivityKind,
  ActivityPage,
  AddTaskInput,
  Backend,
  CommandError,
  DisplayStatus,
  FfmpegStatus,
  LibraryEntry,
  LibraryError,
  LibraryListing,
  DiskSpace,
  UsageSnapshot,
  LiveState,
  Monitoring,
  Platform,
  RecordState,
  SchedulerStatus,
  Settings,
  TaskId,
  TaskMutation,
  TaskSnapshot,
} from "@/lib/api";

const TOTAL_BYTES = 39_636_992;

const ffmpegPresets: Record<string, FfmpegStatus> = {
  checking: { state: "checking" },
  notInstalled: { state: "notInstalled" },
  waiting: { state: "waiting" },
  downloading: {
    state: "downloading",
    downloaded: Math.round(TOTAL_BYTES * 0.42),
    total: TOTAL_BYTES,
    source: "registry.npmmirror.com",
  },
  verifying: { state: "verifying" },
  ready: {
    state: "ready",
    path: "C:\\Users\\me\\AppData\\Local\\com.lubobao.app\\components\\ffmpeg\\9.0.2\\ffmpeg.exe",
    ffmpegVersion: "9.0.2",
    packageVersion: "9.0.2",
    outdated: false,
    installError: null,
  },
  updating: {
    state: "ready",
    path: "C:\\Users\\me\\AppData\\Local\\com.lubobao.app\\components\\ffmpeg\\9.0.1\\ffmpeg.exe",
    ffmpegVersion: "9.0.1",
    packageVersion: "9.0.1",
    outdated: true,
    installError: null,
  },
  outdated: {
    state: "ready",
    path: "C:\\Users\\me\\AppData\\Local\\com.lubobao.app\\components\\ffmpeg\\9.0.1\\ffmpeg.exe",
    ffmpegVersion: "9.0.1",
    packageVersion: "9.0.1",
    outdated: true,
    installError: "两个下载源都无法连接",
  },
  failed: { state: "failed", reason: "两个下载源都无法连接，请检查网络后重试", retryable: true },
  fatal: { state: "failed", reason: "当前系统架构没有可用的 FFmpeg 包", retryable: false },
};

const retryableKinds = new Set(["network", "rate_limited", "parse"]);

function deriveDisplay(
  monitoring: Monitoring,
  live: LiveState,
  record: RecordState,
  notifyOnly: boolean,
): DisplayStatus {
  if (record.state === "recording" || record.state === "finishing") return "recording";
  if (monitoring === "paused") return "paused";
  if (record.state === "failed") return "needsAction";
  switch (live.state) {
    case "checkFailed":
      return retryableKinds.has(live.kind) ? "retrying" : "needsAction";
    case "checking":
      return "checking";
    case "live":
      return notifyOnly ? "notify" : "checking";
    default:
      return "waiting";
  }
}

function displayReason(status: DisplayStatus, live: LiveState, record: RecordState): string | null {
  if (status !== "needsAction" && status !== "retrying") return null;
  if (record.state === "failed") return record.reason;
  if (live.state === "checkFailed") return live.message;
  return null;
}

type MockSeed = Pick<TaskSnapshot, "anchorName" | "roomId" | "monitoring" | "live" | "record"> &
  Partial<Pick<TaskSnapshot, "notifyOnly" | "quality" | "note" | "platform">>;

const mockPlatforms: Record<Platform, { name: string; roomUrl: (roomId: string) => string }> = {
  douyin: { name: "抖音", roomUrl: (roomId) => `https://live.douyin.com/${roomId}` },
  kuaishou: { name: "快手直播", roomUrl: (roomId) => `https://live.kuaishou.com/u/${roomId}` },
};

function snapshot(id: TaskId, seed: MockSeed, createdAt: number): TaskSnapshot {
  const notifyOnly = seed.notifyOnly ?? false;
  const display = deriveDisplay(seed.monitoring, seed.live, seed.record, notifyOnly);
  const platform = seed.platform ?? "douyin";
  const roomUrl = mockPlatforms[platform].roomUrl(seed.roomId);
  return {
    id,
    url: roomUrl,
    platform,
    platformName: mockPlatforms[platform].name,
    roomId: seed.roomId,
    anchorId: platform === "kuaishou" ? String(100_000_000 + id) : `MS4wLjABAAAA-mock-${id}`,
    anchorName: seed.anchorName,
    liveUrl: roomUrl,
    quality: seed.quality ?? "OD",
    notifyOnly,
    note: seed.note ?? "",
    createdAt,
    monitoring: seed.monitoring,
    live: seed.live,
    record: seed.record,
    recordHeld: false,
    display,
    displayReason: displayReason(display, seed.live, seed.record),
  };
}

function seedTasks(now: number): TaskSnapshot[] {
  const minute = 60_000;
  const seeds: MockSeed[] = [
    {
      anchorName: "一只小羊驼",
      roomId: "7381920045",
      monitoring: "enabled",
      live: { state: "live", checkedAt: now - 20_000, title: "晚间闲聊" },
      record: {
        state: "recording",
        filePath: "D:\\Records\\抖音\\一只小羊驼\\一只小羊驼_2026-11-22_20-14-03.ts",
        startedAt: now - 83 * minute,
        bytesWritten: 3_661_000_000,
        bitrateKbps: 6200,
      },
    },
    {
      anchorName: "山野徒步",
      roomId: "21993840",
      monitoring: "enabled",
      quality: "HD",
      live: { state: "checking", since: now - 1_000 },
      record: { state: "idle" },
    },
    {
      anchorName: "峡谷练习生",
      roomId: "canyon_trainee",
      platform: "kuaishou",
      monitoring: "enabled",
      live: { state: "live", checkedAt: now - 40_000, title: null },
      record: {
        state: "recording",
        filePath: "D:\\Records\\快手直播\\峡谷练习生\\峡谷练习生_2026-11-22_21-02-10.ts",
        startedAt: now - 26 * minute,
        bytesWritten: 1_634_000_000,
        bitrateKbps: 8400,
      },
    },
    {
      anchorName: "乡村厨房",
      roomId: "3xcountrykitchen",
      platform: "kuaishou",
      monitoring: "enabled",
      quality: "HD",
      live: { state: "offline", checkedAt: now - 3 * minute },
      record: { state: "idle" },
    },
    {
      anchorName: "甜品研究所",
      roomId: "7210334511",
      monitoring: "enabled",
      live: { state: "offline", checkedAt: now - 2 * minute },
      record: { state: "idle" },
    },
    {
      anchorName: "老王说车",
      roomId: "992817",
      monitoring: "enabled",
      live: {
        state: "checkFailed",
        kind: "network",
        message: "连接超时，30 秒后重试",
        failedAt: now - 12_000,
        nextRetryAt: now + 18_000,
        attempt: 2,
      },
      record: { state: "idle" },
    },
    {
      anchorName: "晚八点读书会",
      roomId: "883021",
      monitoring: "enabled",
      notifyOnly: true,
      quality: "UHD",
      live: { state: "live", checkedAt: now - 30_000, title: "共读《活着》" },
      record: { state: "idle" },
    },
    {
      anchorName: "阿星游戏",
      roomId: "5521",
      monitoring: "enabled",
      live: { state: "live", checkedAt: now - minute, title: null },
      record: {
        state: "failed",
        reason: "磁盘空间不足：D:\\Records 剩余 1.2 GB，低于阈值 2 GB",
        filePath: null,
        failedAt: now - minute,
      },
    },
    {
      anchorName: "私密直播间",
      roomId: "44021",
      monitoring: "enabled",
      live: {
        state: "checkFailed",
        kind: "auth_required",
        message: "该直播间需要登录后观看，请在设置中填写 Cookie",
        failedAt: now - 3 * minute,
        nextRetryAt: null,
        attempt: 1,
      },
      record: { state: "idle" },
    },
    {
      anchorName: "街头篮球",
      roomId: "110293",
      monitoring: "paused",
      live: { state: "unknown" },
      record: { state: "idle" },
      note: "周末再开",
    },
  ];
  return seeds.map((seed, index) => snapshot(index + 1, seed, now - (seeds.length - index) * 86_400_000));
}

const defaultSettings: Settings = {
  live_save_path: "C:\\Users\\me\\Videos\\录播宝",
  loop_time_seconds: 180,
  platform_max_concurrent_requests: 3,
  video_format: "TS",
  record_quality: "OD",
  segmented_recording_enabled: true,
  video_segment_time: 1800,
  recording_space_threshold: 2.0,
  default_live_source: "FLV",
  force_https_recording: true,
  flv_use_direct_download: false,
  convert_to_mp4: true,
  delete_original: false,
  generate_time_subtitle_file: false,
  only_notify_no_record: false,
  filename_includes_title: false,
  remove_emojis: false,
  folder_name_platform: true,
  folder_name_author: true,
  folder_name_time: false,
  folder_name_title: false,
  enable_proxy: true,
  proxy_address: "",
  default_platform_with_proxy:
    "tiktok, sooplive, pandalive, winktv, flextv, popkontv, twitch, liveme, showroom, chzzk, shopee, shp, youtu, youtube, lang",
  execute_custom_script: false,
  custom_script_command: "",
  platform_cookies: {},
  system_notification_enabled: true,
  stream_start_notification_enabled: false,
  stream_end_notification_enabled: false,
};

// ---- 录像库 ----

/** 目录：子项名称 → 子目录或文件。文件为 [字节数, 距今分钟数]。 */
interface MockDir {
  [name: string]: MockDir | MockFile;
}
type MockFile = [number, number];

const GB = 1024 ** 3;
const MB = 1024 ** 2;

function segments(prefix: string, count: number, ageMinutes: number, bytes: number): MockDir {
  const dir: MockDir = {};
  for (let i = 0; i < count; i++) {
    dir[`${prefix}_${String(i).padStart(3, "0")}.ts`] = [bytes, ageMinutes - i * 30];
  }
  return dir;
}

function mockLibraryTree(): MockDir {
  return {
    抖音: {
      一只小羊驼: {
        // 正在录制的一次，最后一段仍在写入。
        ...segments("一只小羊驼_2026-11-22_20-14-03", 3, 83, 1.3 * GB),
        ...segments("一只小羊驼_2026-11-21_20-01-47", 5, 1_500, 1.4 * GB),
        "一只小羊驼_2026-11-20_19-58-12.mp4": [5.62 * GB, 2_900],
      },
      甜品研究所: {
        "甜品研究所_2026-11-19_14-00-05_000.ts": [820 * MB, 4_600],
        "甜品研究所_2026-11-18_14-02-44_000.ts": [1.1 * GB, 6_100],
      },
      晚八点读书会: {},
      // 大目录：800 个分段，用来检查虚拟滚动。
      长时间主播: {
        ...segments("长时间主播_2026-11-10_08-00-00", 500, 20_000, 1.2 * GB),
        ...segments("长时间主播_2026-11-01_08-00-00", 300, 40_000, 1.2 * GB),
      },
      阿星游戏: {
        "阿星游戏_2026-11-22_21-01-30_000.ts": [312 * MB, 22],
      },
    },
    快手: {
      夜听电台: {
        "夜听电台 第 12 期_2026-11-21_23-40-00_000.ts": [2.2 * GB, 1_300],
        "夜听电台 第 12 期_2026-11-21_23-40-00_001.ts": [640 * MB, 1_270],
      },
    },
    空目录: {},
    "手动转存 (旧).mp4": [3.4 * GB, 20_000],
  };
}

function isMockDir(node: MockDir | MockFile): node is MockDir {
  return !Array.isArray(node);
}

function splitSegment(name: string): [string, number | null] {
  const match = /^(.+)_(\d{3,})(\.[^.]+)$/.exec(name);
  return match ? [match[1] + match[3], Number(match[2])] : [name, null];
}

/** 与 src-tauri/src/library 的排序一致：目录按名称；文件按同次录制分组，组按最新时间倒序，组内按序号。 */
function sortEntries(entries: LibraryEntry[]): LibraryEntry[] {
  const dirs = entries
    .filter((entry) => entry.kind === "dir")
    .sort((a, b) => a.name.localeCompare(b.name, "zh-CN"));
  const files = entries.filter((entry) => entry.kind === "file");
  const latest = new Map<string, number>();
  for (const file of files) {
    const key = file.group ?? file.name;
    latest.set(key, Math.max(latest.get(key) ?? -Infinity, file.modifiedAt ?? -Infinity));
  }
  const groupOf = (entry: LibraryEntry) => entry.group ?? entry.name;
  files.sort(
    (a, b) =>
      (latest.get(groupOf(b)) ?? 0) - (latest.get(groupOf(a)) ?? 0) ||
      groupOf(b).localeCompare(groupOf(a)) ||
      (a.segment ?? 0) - (b.segment ?? 0) ||
      a.name.localeCompare(b.name),
  );
  return [...dirs, ...files];
}

function walkMock(dir: MockDir): { bytes: number; files: number } {
  let bytes = 0;
  let files = 0;
  for (const node of Object.values(dir)) {
    if (isMockDir(node)) {
      const child = walkMock(node);
      bytes += child.bytes;
      files += child.files;
    } else {
      bytes += node[0];
      files += 1;
    }
  }
  return { bytes, files };
}

// ---- 动态历史 ----

interface ActivitySeed {
  task: TaskSnapshot | null;
  kind: ActivityKind;
  message: string;
  detail?: Record<string, unknown>;
}

/** 三天的历史动态，按时间从旧到新返回。条数超过一页（50），方便检查分页。 */
function seedActivity(tasks: TaskSnapshot[], now: number): { at: number; seed: ActivitySeed }[] {
  const byName = (name: string) => tasks.find((task) => task.anchorName === name) ?? null;
  const minute = 60_000;
  const result: { at: number; seed: ActivitySeed }[] = [];
  const push = (minutesAgo: number, seed: ActivitySeed) =>
    result.push({ at: now - minutesAgo * minute, seed });

  for (const task of tasks) push(4_400 - task.id * 3, { task, kind: "user_action", message: "添加任务" });
  // 已删除的任务：动态保留，taskId 为空。
  push(4_300, { task: null, kind: "user_action", message: "删除任务" });

  // 前两天：每个录制任务每天开播、录制、重试、下播各一轮。
  for (const day of [2, 1]) {
    const base = day * 1_440;
    for (const [offset, name] of [
      [120, "一只小羊驼"],
      [260, "甜品研究所"],
      [300, "阿星游戏"],
      [330, "山野徒步"],
    ] as const) {
      const task = byName(name);
      const start = base + offset;
      const filePath = `C:\\Users\\me\\Videos\\录播宝\\抖音\\${name}\\${name}_000.ts`;
      push(start, { task, kind: "live_start", message: "检测到开播", detail: { title: "今晚见" } });
      push(start - 1, { task, kind: "record_start", message: "开始录制", detail: { filePath, quality: "OD" } });
      for (const retry of [0, 1]) {
        push(start - 20 - retry * 15, {
          task,
          kind: "failure",
          message: "检测失败：连接超时",
          detail: { kind: "network" },
        });
      }
      push(start - 110, { task, kind: "live_end", message: "已下播" });
      push(start - 111, {
        task,
        kind: "record_end",
        message: "录制结束",
        detail: { filePath, durationMs: 110 * minute },
      });
      const mp4 = filePath.replace(/\.ts$/, ".mp4");
      push(start - 112, { task, kind: "convert", message: "开始转为 MP4", detail: { filePath, targetPath: mp4 } });
      push(start - 113, {
        task,
        kind: "convert",
        message: "已转为 MP4",
        detail: { filePath: mp4, sourcePath: filePath, durationSecs: 110 * 60, originalDeleted: false },
      });
    }
    push(base + 30, { task: byName("街头篮球"), kind: "user_action", message: "暂停监控" });
    push(base + 5, { task: byName("晚八点读书会"), kind: "live_start", message: "检测到开播" });
  }

  // 今天。
  push(200, {
    task: byName("晚八点读书会"),
    kind: "live_start",
    message: "检测到开播",
    detail: { title: "共读《活着》" },
  });
  push(84, { task: byName("一只小羊驼"), kind: "live_start", message: "检测到开播", detail: { title: "晚间闲聊" } });
  push(83, {
    task: byName("一只小羊驼"),
    kind: "record_start",
    message: "开始录制",
    detail: { filePath: "C:\\Users\\me\\Videos\\录播宝\\抖音\\一只小羊驼\\一只小羊驼_2026-11-22_20-14-03_000.ts", quality: "OD" },
  });
  push(40, {
    task: byName("私密直播间"),
    kind: "failure",
    message: "检测失败：该直播间需要登录后观看，请在设置中填写 Cookie",
    detail: { kind: "auth_required" },
  });
  push(22, { task: byName("阿星游戏"), kind: "record_start", message: "开始录制", detail: { quality: "OD" } });
  push(1, {
    task: byName("阿星游戏"),
    kind: "failure",
    message: "录制失败：磁盘空间不足：D:\\Records 剩余 1.2 GB，低于阈值 2 GB",
    detail: { filePath: null },
  });
  push(0.2, {
    task: byName("老王说车"),
    kind: "failure",
    message: "检测失败：连接超时，30 秒后重试",
    detail: { kind: "network" },
  });

  return result.sort((a, b) => a.at - b.at);
}

type Handler<T> = (payload: T) => void;

class Channel<T> {
  private handlers = new Set<Handler<T>>();

  subscribe(handler: Handler<T>): Promise<UnlistenFn> {
    this.handlers.add(handler);
    return Promise.resolve(() => {
      this.handlers.delete(handler);
    });
  }

  emit(payload: T) {
    for (const handler of this.handlers) handler(payload);
  }
}

function reject(kind: CommandError["kind"], message: string): Promise<never> {
  const error: CommandError = { kind, message };
  return Promise.reject(error);
}

function delay(ms: number) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

export function createMockBackend(): Backend {
  const now = Date.now();
  let tasks = seedTasks(now);
  let nextId = tasks.length + 1;
  let settings = { ...defaultSettings };
  const activity: ActivityEntry[] = [];
  let nextActivityId = 1;

  const ffmpegChannel = new Channel<FfmpegStatus>();
  const updatedChannel = new Channel<TaskSnapshot>();
  const removedChannel = new Channel<{ ids: TaskId[] }>();
  const activityChannel = new Channel<ActivityEntry>();
  const schedulerChannel = new Channel<SchedulerStatus>();

  // 假调度：每 180 秒一轮检测。
  const MOCK_INTERVAL_MS = 180_000;
  let nextCheckAt = now + 42_000;
  function schedulerStatus(): SchedulerStatus {
    return {
      nextCheckAt: tasks.some((task) => task.monitoring === "enabled") ? nextCheckAt : null,
      checking: 0,
      recording: tasks.filter((task) => task.display === "recording").length,
    };
  }
  setInterval(() => {
    if (Date.now() < nextCheckAt) return;
    nextCheckAt += MOCK_INTERVAL_MS;
    schedulerChannel.emit(schedulerStatus());
  }, 1000);

  let ffmpeg: FfmpegStatus = { state: "checking" };
  let simulation: ReturnType<typeof setInterval> | undefined;

  function setFfmpeg(status: FfmpegStatus) {
    ffmpeg = status;
    ffmpegChannel.emit(status);
  }

  function simulateDownload() {
    clearInterval(simulation);
    let downloaded = 0;
    setFfmpeg({ state: "checking" });
    simulation = setInterval(() => {
      downloaded += TOTAL_BYTES / 20;
      if (downloaded < TOTAL_BYTES) {
        setFfmpeg({
          state: "downloading",
          downloaded: Math.round(downloaded),
          total: TOTAL_BYTES,
          source: "registry.npmmirror.com",
        });
        return;
      }
      clearInterval(simulation);
      setFfmpeg({ state: "verifying" });
      setTimeout(() => setFfmpeg(ffmpegPresets.ready), 1200);
    }, 400);
  }

  const preset = new URLSearchParams(window.location.search).get("ffmpeg");
  if (preset && preset in ffmpegPresets) {
    ffmpeg = ffmpegPresets[preset];
  } else {
    setTimeout(simulateDownload, 800);
  }

  function log(task: TaskSnapshot | null, kind: ActivityEntry["kind"], message: string) {
    const entry: ActivityEntry = {
      id: nextActivityId++,
      at: Date.now(),
      taskId: task?.id ?? null,
      anchorName: task?.anchorName ?? "",
      kind,
      message,
      detail: null,
    };
    activity.unshift(entry);
    activityChannel.emit(entry);
  }

  function replace(task: TaskSnapshot) {
    tasks = tasks.map((item) => (item.id === task.id ? task : item));
    updatedChannel.emit(task);
  }

  function rederive(task: TaskSnapshot, patch: Partial<TaskSnapshot>): TaskSnapshot {
    const next = { ...task, ...patch };
    // 与 Rust 一致：手动停止后仍在直播时按仅通知显示。
    next.display = deriveDisplay(
      next.monitoring,
      next.live,
      next.record,
      next.notifyOnly || next.recordHeld,
    );
    next.displayReason =
      next.display === "notify" && next.recordHeld && !next.notifyOnly
        ? "已手动停止录制，下次开播时自动录制"
        : displayReason(next.display, next.live, next.record);
    return next;
  }

  for (const { at, seed } of seedActivity(tasks, now)) {
    activity.unshift({
      id: nextActivityId++,
      at,
      taskId: seed.task?.id ?? null,
      anchorName: seed.task?.anchorName ?? "旧任务",
      kind: seed.kind,
      message: seed.message,
      detail: seed.detail ?? null,
    });
  }

  const libraryTree = mockLibraryTree();
  let usageCache: { usage: { usedBytes: number; fileCount: number }; computedAt: number } | null = null;
  let usageRunning = false;
  let libraryForcedMissing = new URLSearchParams(window.location.search).get("library") === "missing";

  const libraryExists = () =>
    !libraryForcedMissing && settings.live_save_path === defaultSettings.live_save_path;

  function libraryFailure(kind: LibraryError["kind"], message: string): LibraryError {
    return { kind, message, root: settings.live_save_path };
  }

  /** 解析相对路径，规则与 Rust 一致：拒绝 ..、绝对路径和带 : 的片段。 */
  function libraryLookup(path: string): { parts: string[]; node: MockDir | MockFile } {
    const root = settings.live_save_path;
    if (!libraryExists()) throw libraryFailure("root_missing", `录像目录不存在：${root}`);
    if (/^[\\/]/.test(path)) throw libraryFailure("invalid_path", "路径必须相对于录像目录");
    const parts = path.split(/[\\/]/).filter((part) => part !== "" && part !== ".");
    let node: MockDir | MockFile = libraryTree;
    for (const part of parts) {
      if (part === ".." || part.includes(":")) throw libraryFailure("invalid_path", `路径片段无效：${part}`);
      const next: MockDir | MockFile | undefined = isMockDir(node) ? node[part] : undefined;
      if (!next) {
        throw libraryFailure("not_found", `找不到 ${root}\\${parts.join("\\")}，可能已被移动或删除`);
      }
      node = next;
    }
    return { parts, node };
  }

  const backend: Backend = {
    getFfmpegStatus: () => Promise.resolve(ffmpeg),
    retryFfmpegInstall: () => {
      simulateDownload();
      return Promise.resolve(ffmpeg);
    },
    listTasks: () => Promise.resolve(tasks),

    async addTask(input: AddTaskInput): Promise<TaskMutation> {
      await delay(600);
      const url = input.url.trim();
      const match = /live\.douyin\.com\/([\w.-]+)/.exec(url);
      if (!url) return reject("invalid_input", "请输入直播间链接");
      if (!match) return reject("unsupported_platform", `暂不支持这个链接的平台：${url}`);
      const roomId = match[1];
      const existing = tasks.filter((task) => task.roomId === roomId);
      const candidate = {
        url,
        platform: "douyin" as const,
        platformName: "抖音",
        roomId,
        anchorId: `MS4wLjABAAAA-mock-${roomId}`,
        anchorName: `主播 ${roomId}`,
        liveUrl: `https://live.douyin.com/${roomId}`,
        isLive: false,
        title: null,
      };
      if (existing.length > 0 && !input.force) {
        return { result: "duplicate", existing, candidate };
      }
      const task = snapshot(
        nextId++,
        {
          anchorName: candidate.anchorName,
          roomId,
          monitoring: "enabled",
          live: { state: "offline", checkedAt: Date.now() },
          record: { state: "idle" },
          quality: input.quality ?? settings.record_quality,
          notifyOnly: input.notifyOnly ?? settings.only_notify_no_record,
          note: input.note ?? "",
        },
        Date.now(),
      );
      task.url = url;
      tasks = [...tasks, task];
      updatedChannel.emit(task);
      log(task, "user_action", "添加任务");
      return { result: "added", task };
    },

    async updateTask(input) {
      const task = tasks.find((item) => item.id === input.id);
      if (!task) return reject("not_found", `任务 ${input.id} 不存在`);
      const next = rederive(task, {
        ...(input.url !== undefined && { url: input.url.trim() }),
        ...(input.quality !== undefined && { quality: input.quality }),
        ...(input.notifyOnly !== undefined && { notifyOnly: input.notifyOnly }),
        ...(input.note !== undefined && { note: input.note }),
      });
      replace(next);
      log(next, "user_action", "修改任务");
      return { result: "updated", task: next };
    },

    async deleteTasks(ids) {
      const busy = tasks.find(
        (task) =>
          ids.includes(task.id) &&
          (task.record.state === "recording" || task.record.state === "finishing"),
      );
      if (busy) return reject("recording_active", `${busy.anchorName} 正在录制，停止录制后才能删除`);
      const removed = tasks.filter((task) => ids.includes(task.id));
      tasks = tasks.filter((task) => !ids.includes(task.id));
      if (removed.length > 0) removedChannel.emit({ ids: removed.map((task) => task.id) });
      for (const task of removed) log(task, "user_action", "删除任务");
      return removed.map((task) => task.id);
    },

    async setMonitoring(ids, enabled) {
      const monitoring: Monitoring = enabled ? "enabled" : "paused";
      const result: TaskSnapshot[] = [];
      for (const task of tasks.filter((item) => ids.includes(item.id))) {
        if (task.monitoring === monitoring) {
          result.push(task);
          continue;
        }
        // 与 Rust 一致：暂停监控时停止录制（假数据直接结束）；重新启用时清除录制失败和手动停止。
        const recording = task.record.state === "recording" || task.record.state === "finishing";
        const record: RecordState =
          (!enabled && recording) || (enabled && task.record.state === "failed")
            ? { state: "idle" }
            : task.record;
        const next = rederive(task, {
          monitoring,
          live: { state: "unknown" },
          record,
          recordHeld: enabled ? false : task.recordHeld,
        });
        replace(next);
        log(next, "user_action", enabled ? "开始监控" : "暂停监控");
        if (!enabled && recording) log(next, "record_end", "录制结束");
        result.push(next);
      }
      return result;
    },

    async stopRecording(ids) {
      const result: TaskSnapshot[] = [];
      for (const id of ids) {
        const task = tasks.find((item) => item.id === id);
        if (!task) continue;
        if (task.record.state !== "recording") {
          result.push(task);
          continue;
        }
        // 与 Rust 一致：先进入收尾中，ffmpeg 写完文件尾后回到空闲。
        const { filePath, startedAt } = task.record;
        const finishing = rederive(task, {
          record: { state: "finishing", filePath, startedAt },
          recordHeld: true,
        });
        replace(finishing);
        setTimeout(() => {
          const current = tasks.find((item) => item.id === id);
          if (current?.record.state !== "finishing") return;
          const idle = rederive(current, { record: { state: "idle" } });
          replace(idle);
          log(idle, "record_end", "录制结束");
          if (settings.convert_to_mp4) {
            log(idle, "convert", "开始转为 MP4");
            setTimeout(() => log(idle, "convert", "已转为 MP4"), 1000);
          }
          schedulerChannel.emit(schedulerStatus());
        }, 1500);
        result.push(finishing);
      }
      schedulerChannel.emit(schedulerStatus());
      return result;
    },

    getSettings: () => Promise.resolve(settings),
    async updateSettings(values) {
      // 与 Rust 一致：Cookie 不能含换行等控制字符（制表符除外）。
      for (const [platform, cookie] of Object.entries(values.platform_cookies ?? {})) {
        // eslint-disable-next-line no-control-regex
        if (typeof cookie !== "string" || /[\x00-\x08\x0a-\x1f\x7f]/.test(cookie)) {
          return reject("config", `${platform} 的 Cookie 含有换行等无法放进请求头的字符`);
        }
      }
      settings = { ...settings, ...values };
      return settings;
    },

    async listActivity(query = {}): Promise<ActivityPage> {
      const limit = Math.min(Math.max(query.limit ?? 50, 1), 200);
      const matching = activity.filter(
        (entry) =>
          (query.taskId === undefined || entry.taskId === query.taskId) &&
          (query.before === undefined || entry.id < query.before) &&
          (!query.kinds?.length || query.kinds.includes(entry.kind)),
      );
      const items = matching.slice(0, limit);
      return {
        items,
        nextBefore: matching.length > limit ? items[items.length - 1].id : null,
      };
    },

    async listLibrary(path = ""): Promise<LibraryListing> {
      await delay(150);
      const found = libraryLookup(path);
      if (!isMockDir(found.node)) throw libraryFailure("invalid_target", `${path} 不是文件夹`);
      const prefix = found.parts.length ? `${found.parts.join("/")}/` : "";
      const entries = Object.entries(found.node).map(([name, node]): LibraryEntry => {
        if (isMockDir(node)) {
          return {
            name,
            path: prefix + name,
            kind: "dir",
            size: null,
            modifiedAt: now - 30 * 60_000,
            childCount: Object.keys(node).length,
            segment: null,
            group: null,
          };
        }
        const [group, segment] = splitSegment(name);
        return {
          name,
          path: prefix + name,
          kind: "file",
          size: Math.round(node[0]),
          modifiedAt: now - node[1] * 60_000,
          childCount: null,
          segment,
          group,
        };
      });
      return { root: settings.live_save_path, path: found.parts.join("/"), entries: sortEntries(entries) };
    },

    async getDiskSpace(): Promise<DiskSpace> {
      await delay(80);
      return {
        root: settings.live_save_path,
        rootExists: libraryExists(),
        availableBytes: Math.round(186.4 * GB),
        totalBytes: Math.round(931.5 * GB),
      };
    },

    async getLibraryUsage(refresh = false): Promise<UsageSnapshot> {
      await delay(80);
      const exists = libraryExists();
      if (!exists) {
        return { root: settings.live_save_path, rootExists: false, usage: null, computedAt: null, computing: false };
      }
      // 模拟 Rust 的缓存：过期或刷新时启动一次 1.5 秒的「遍历」，期间返回上次结果并标明统计中。
      const stale = !usageCache || Date.now() - usageCache.computedAt > 30_000;
      if (!usageRunning && (refresh || stale)) {
        usageRunning = true;
        setTimeout(() => {
          const walked = walkMock(libraryTree);
          usageCache = {
            usage: { usedBytes: Math.round(walked.bytes), fileCount: walked.files },
            computedAt: Date.now(),
          };
          usageRunning = false;
        }, 1500);
      }
      return {
        root: settings.live_save_path,
        rootExists: true,
        usage: usageCache?.usage ?? null,
        computedAt: usageCache?.computedAt ?? null,
        computing: usageRunning,
      };
    },

    async openLibraryFile(path) {
      const found = libraryLookup(path);
      if (isMockDir(found.node)) throw libraryFailure("invalid_target", `${path} 不是录像文件`);
      console.info("[mock] 用系统播放器打开", path);
    },

    async openLibraryFolder(path) {
      const found = libraryLookup(path);
      if (!isMockDir(found.node)) throw libraryFailure("invalid_target", `${path} 不是文件夹`);
      console.info("[mock] 打开文件夹", path || "（根目录）");
    },

    async revealLibraryItem(path) {
      libraryLookup(path);
      console.info("[mock] 在文件夹中显示", path || "（根目录）");
    },

    onFfmpegStatus: (handler) => ffmpegChannel.subscribe(handler),
    onTaskUpdated: (handler) => updatedChannel.subscribe(handler),
    onTaskRemoved: (handler) => removedChannel.subscribe(handler),
    onActivity: (handler) => activityChannel.subscribe(handler),
    getSchedulerStatus: () => Promise.resolve(schedulerStatus()),
    onSchedulerStatus: (handler) => schedulerChannel.subscribe(handler),
  };

  (window as unknown as { __lubobaoMock: unknown }).__lubobaoMock = {
    setFfmpeg: (name: string) => {
      clearInterval(simulation);
      const status = ffmpegPresets[name];
      if (status) setFfmpeg(status);
    },
    simulateDownload,
    /** 切换录像根目录是否存在。 */
    setLibraryMissing: (missing: boolean) => {
      libraryForcedMissing = missing;
    },
    /** 模拟一条新动态，检查实时插入。 */
    pushActivity: (kind: ActivityKind = "live_start") => {
      const task = tasks[Math.floor(Math.random() * tasks.length)] ?? null;
      const messages: Record<ActivityKind, string> = {
        live_start: "检测到开播",
        live_end: "已下播",
        record_start: "开始录制",
        record_end: "录制结束",
        failure: "检测失败：连接超时",
        warning: "代理是 SOCKS，录制不支持，本次直连录制",
        user_action: "修改任务",
        convert: "已转为 MP4",
      };
      log(task, kind, messages[kind]);
    },
    backend,
  };

  return backend;
}
