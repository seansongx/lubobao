// 浏览器假数据（npm run dev，不在 Tauri 里时使用），方便调界面。
// 任务覆盖七种显示状态；FFmpeg 状态可用地址参数切换：
//   ?ffmpeg=checking | notInstalled | waiting | downloading | verifying | ready | updating | outdated | failed | fatal
// 缺省时模拟一次下载：下载中 → 校验 → 可用。控制台可调用 window.__lubobaoMock.setFfmpeg("failed")。
// 显示状态的推导复刻 src-tauri/src/tasks/model.rs 的 derive_display，只用于假数据。
//
// 添加任务的演示链接（见 resolveMock）：
//   live.douyin.com/{数字或抖音号}    识别成功；同一房间号或同一主播判为重复
//   v.douyin.com/CeiU5cbX            主页短链，与 live.douyin.com/745964462470 是同一主播
//   v.douyin.com/{其他}               识别为列表中第一个主播（演示重复）
//   www.douyin.com/user/{sec_uid}    主页链接
//   路径含 login / timeout / busy / parse  分别返回 auth_required / network / rate_limited / parse
//   其他域名                          unsupported_platform；无法解析的文本为 invalid_url
// 事件推送：录制中的任务每秒增长大小；等待开播的任务每隔一段时间模拟一次检测；重试中的任务按 nextRetryAt 重试。
// 任务列表：?tasks=slow 延迟 1.5 秒返回；?tasks=fail 第一次读取失败。
// 控制台：window.__lubobaoMock.goLive(id) / endLive(id) / setStopSupported(true) / setEvents(false)。

import type { UnlistenFn } from "@tauri-apps/api/event";

import type {
  ActivityEntry,
  ActivityPage,
  AddTaskInput,
  Backend,
  CommandError,
  DisplayStatus,
  FfmpegStatus,
  LiveState,
  Monitoring,
  RecordState,
  ResolvedRoom,
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
  Partial<Pick<TaskSnapshot, "notifyOnly" | "quality" | "note" | "anchorId" | "url">>;

function snapshot(id: TaskId, seed: MockSeed, createdAt: number): TaskSnapshot {
  const notifyOnly = seed.notifyOnly ?? false;
  const display = deriveDisplay(seed.monitoring, seed.live, seed.record, notifyOnly);
  return {
    id,
    url: seed.url ?? `https://live.douyin.com/${seed.roomId}`,
    platform: "douyin",
    platformName: "抖音",
    roomId: seed.roomId,
    anchorId: seed.anchorId ?? `MS4wLjABAAAA-mock-${id}`,
    anchorName: seed.anchorName,
    liveUrl: `https://live.douyin.com/${seed.roomId}`,
    quality: seed.quality ?? "OD",
    notifyOnly,
    note: seed.note ?? "",
    createdAt,
    monitoring: seed.monitoring,
    live: seed.live,
    record: seed.record,
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
  execute_custom_script: false,
  custom_script_command: "",
  system_notification_enabled: true,
  stream_start_notification_enabled: false,
  stream_end_notification_enabled: false,
};

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

function normalizeUrl(url: string): string {
  return url
    .trim()
    .replace(/^https?:\/\//i, "")
    .replace(/[?#].*$/, "")
    .replace(/\/+$/, "")
    .toLowerCase();
}

/** 短链 CeiU5cbX 与 745964462470 是同一主播（对应真实验证用的链接）。 */
const DEMO_ROOM = "745964462470";
const DEMO_ANCHOR = `MS4wLjABAAAA-mock-anchor-${DEMO_ROOM}`;

/** 模拟 platforms::resolve：只认抖音，按链接内容返回各类错误。 */
function resolveMock(input: string, tasks: TaskSnapshot[]): ResolvedRoom | CommandError {
  const url = input.trim();
  if (!url) return { kind: "invalid_input", message: "请输入直播间链接" };
  let parsed: URL;
  try {
    parsed = new URL(/^https?:\/\//i.test(url) ? url : `https://${url}`);
  } catch {
    return { kind: "invalid_url", message: `无法识别的链接：${url}` };
  }
  const host = parsed.hostname.toLowerCase();
  const path = parsed.pathname;
  if (!host.includes(".")) return { kind: "invalid_url", message: `无法识别的链接：${url}` };
  if (!host.endsWith("douyin.com")) return { kind: "unsupported_platform", message: `暂不支持 ${host} 的直播间` };
  if (/login/i.test(path)) return { kind: "auth_required", message: "该直播间需要登录后观看，请填写 Cookie" };
  if (/timeout|network/i.test(path)) return { kind: "network", message: "连接 live.douyin.com 超时" };
  if (/busy/i.test(path)) return { kind: "rate_limited", message: "当前服务繁忙（10011），请稍后再试" };
  if (/parse/i.test(path)) return { kind: "parse", message: "直播间页面缺少 roomStore 数据" };

  const base = { url, platform: "douyin" as const, platformName: "抖音", isLive: false, title: null };
  if (host === "v.douyin.com") {
    const code = path.split("/").filter(Boolean)[0];
    if (!code) return { kind: "invalid_url", message: "短链缺少短码" };
    if (code === "CeiU5cbX") {
      return {
        ...base,
        roomId: DEMO_ROOM,
        anchorId: DEMO_ANCHOR,
        anchorName: `主播 ${DEMO_ROOM}`,
        liveUrl: `https://live.douyin.com/${DEMO_ROOM}`,
      };
    }
    const first = tasks[0];
    if (!first) return { kind: "invalid_url", message: "短链已失效" };
    return {
      ...base,
      roomId: first.roomId,
      anchorId: first.anchorId,
      anchorName: first.anchorName,
      liveUrl: first.liveUrl,
    };
  }
  const user = /^\/user\/([\w-]+)/.exec(path);
  if (user) {
    return {
      ...base,
      roomId: user[1].slice(0, 12),
      anchorId: user[1],
      anchorName: `主播 ${user[1].slice(0, 6)}`,
      liveUrl: null,
    };
  }
  const room = /^\/([\w.-]+)\/?$/.exec(path);
  if (host === "live.douyin.com" && room) {
    const roomId = room[1];
    const known = tasks.find((task) => task.roomId === roomId);
    return {
      ...base,
      roomId,
      anchorId: known?.anchorId ?? (roomId === DEMO_ROOM ? DEMO_ANCHOR : `MS4wLjABAAAA-mock-anchor-${roomId}`),
      anchorName: known?.anchorName ?? `主播 ${roomId}`,
      liveUrl: `https://live.douyin.com/${roomId}`,
    };
  }
  return { kind: "invalid_url", message: `无法从链接中找到直播间：${url}` };
}

/** 与 src-tauri/src/tasks/dedup.rs 一致：同平台 anchor_id 相同，或 url/live_url 归一化后相同。 */
function findDuplicates(candidate: ResolvedRoom, tasks: TaskSnapshot[], exclude?: TaskId): TaskSnapshot[] {
  const urls = [candidate.url, candidate.liveUrl].filter((value): value is string => !!value).map(normalizeUrl);
  return tasks.filter((task) => {
    if (task.id === exclude || task.platform !== candidate.platform) return false;
    if (candidate.anchorId && task.anchorId === candidate.anchorId) return true;
    const own = [task.url, task.liveUrl].filter((value): value is string => !!value).map(normalizeUrl);
    return own.some((value) => urls.includes(value));
  });
}

const qualityNames: Record<TaskSnapshot["quality"], string> = { OD: "原画", UHD: "超清", HD: "高清", SD: "标清", LD: "流畅" };

function isActive(record: RecordState) {
  return record.state === "recording" || record.state === "finishing";
}

export function createMockBackend(): Backend {
  const now = Date.now();
  let tasks = seedTasks(now);
  let nextId = tasks.length + 1;
  let settings = { ...defaultSettings };
  const activity: ActivityEntry[] = [];
  let nextActivityId = 1;
  let stopSupported = false;
  const tasksMode = new URLSearchParams(window.location.search).get("tasks");
  // 状态栏和页面各订阅一次，两次都失败才能稳定演示。
  let failuresLeft = tasksMode === "fail" ? 2 : 0;
  let eventsEnabled = true;

  const ffmpegChannel = new Channel<FfmpegStatus>();
  const updatedChannel = new Channel<TaskSnapshot>();
  const removedChannel = new Channel<{ ids: TaskId[] }>();
  const activityChannel = new Channel<ActivityEntry>();

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

  function log(task: TaskSnapshot | null, kind: ActivityEntry["kind"], message: string, at = Date.now()) {
    const entry: ActivityEntry = {
      id: nextActivityId++,
      at,
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
    next.display = deriveDisplay(next.monitoring, next.live, next.record, next.notifyOnly);
    next.displayReason = displayReason(next.display, next.live, next.record);
    return next;
  }

  function patchTask(id: TaskId, patch: Partial<TaskSnapshot>): TaskSnapshot | null {
    const task = tasks.find((item) => item.id === id);
    if (!task) return null;
    const next = rederive(task, patch);
    replace(next);
    return next;
  }

  // 种子动态：按时间排序，最新的在最前，id 也按时间递增（与 before 分页一致）。
  const seedEntries: Omit<ActivityEntry, "id">[] = [];
  const seed = (task: TaskSnapshot, kind: ActivityEntry["kind"], message: string, at: number) =>
    seedEntries.push({ at, taskId: task.id, anchorName: task.anchorName, kind, message, detail: null });
  for (const task of tasks) {
    const { live, record } = task;
    seed(task, "user_action", "添加任务", task.createdAt);
    if (record.state === "recording") {
      seed(task, "live_start", live.state === "live" && live.title ? `开播：${live.title}` : "开播", record.startedAt - 4_000);
      seed(task, "record_start", `开始录制 · ${qualityNames[task.quality]}`, record.startedAt);
    } else if (record.state === "failed") {
      seed(task, "live_start", "开播", record.failedAt - 20 * 60_000);
      seed(task, "record_start", "开始录制", record.failedAt - 20 * 60_000 + 3_000);
      seed(task, "failure", `录制中断：${record.reason}`, record.failedAt);
    } else if (live.state === "checkFailed") {
      seed(task, "failure", `检测失败：${live.message}`, live.failedAt);
    } else if (live.state === "live") {
      seed(task, "live_start", live.title ? `开播：${live.title}` : "开播", live.checkedAt);
    } else if (task.monitoring === "paused") {
      seed(task, "user_action", "暂停监控", task.createdAt + 3_600_000);
    }
  }
  seedEntries.sort((a, b) => a.at - b.at);
  for (const entry of seedEntries) activity.unshift({ ...entry, id: nextActivityId++ });

  function startRecording(task: TaskSnapshot) {
    const started = Date.now();
    const stamp = new Date(started).toISOString().slice(0, 19).replace("T", "_").replace(/:/g, "-");
    return patchTask(task.id, {
      record: {
        state: "recording",
        filePath: `${settings.live_save_path}\\抖音\\${task.anchorName}\\${task.anchorName}_${stamp}.ts`,
        startedAt: started,
        bytesWritten: 0,
        bitrateKbps: 4000 + Math.round(Math.random() * 3000),
      },
    });
  }

  function goLive(id: TaskId) {
    const task = tasks.find((item) => item.id === id);
    if (!task || task.monitoring === "paused") return;
    const next = patchTask(id, { live: { state: "live", checkedAt: Date.now(), title: "演示直播" } });
    if (!next) return;
    log(next, "live_start", "开播：演示直播");
    if (!next.notifyOnly && !isActive(next.record)) {
      const recording = startRecording(next);
      if (recording) log(recording, "record_start", `开始录制 · ${qualityNames[recording.quality]}`);
    }
  }

  function finishRecording(id: TaskId, reason: string) {
    const task = tasks.find((item) => item.id === id);
    if (!task || !isActive(task.record)) return;
    patchTask(id, {
      record: { state: "finishing", filePath: task.record.filePath, startedAt: task.record.startedAt },
    });
    setTimeout(() => {
      const current = tasks.find((item) => item.id === id);
      if (!current || current.record.state !== "finishing") return;
      const done = patchTask(id, { record: { state: "idle" } });
      if (done) log(done, "record_end", reason);
    }, 1200);
  }

  function endLive(id: TaskId) {
    const task = tasks.find((item) => item.id === id);
    if (!task) return;
    finishRecording(id, "下播，录制结束");
    const next = patchTask(id, { live: { state: "offline", checkedAt: Date.now() } });
    if (next && task.live.state === "live") log(next, "live_end", "下播");
  }

  // 事件推送：录制进度每秒更新；等待开播的任务定期检测；重试中的任务到点重试。
  let tick = 0;
  setInterval(() => {
    if (!eventsEnabled) return;
    tick += 1;
    const at = Date.now();
    for (const task of tasks) {
      const { record, live } = task;
      if (record.state === "recording") {
        const kbps = Math.round(record.bitrateKbps * (0.97 + Math.random() * 0.06));
        patchTask(task.id, {
          record: { ...record, bytesWritten: record.bytesWritten + Math.round((kbps * 1000) / 8), bitrateKbps: kbps },
        });
        continue;
      }
      if (task.monitoring === "paused") continue;
      if (live.state === "offline" && tick % 12 === task.id % 12) {
        patchTask(task.id, { live: { state: "checking", since: at } });
        setTimeout(() => {
          const current = tasks.find((item) => item.id === task.id);
          if (current?.live.state === "checking") {
            patchTask(task.id, { live: { state: "offline", checkedAt: Date.now() } });
          }
        }, 1500);
      } else if (live.state === "checkFailed" && live.nextRetryAt !== null && live.nextRetryAt <= at) {
        patchTask(task.id, { live: { ...live, attempt: live.attempt + 1, failedAt: at, nextRetryAt: at + 30_000 } });
      }
    }
  }, 1000);

  const backend: Backend = {
    getFfmpegStatus: () => Promise.resolve(ffmpeg),
    retryFfmpegInstall: () => {
      simulateDownload();
      return Promise.resolve(ffmpeg);
    },
    async listTasks() {
      // ?tasks=slow 模拟加载耗时；?tasks=fail 让第一次读取失败，用来演示重试。
      if (tasksMode === "slow") await delay(1500);
      if (failuresLeft > 0) {
        failuresLeft -= 1;
        await delay(600);
        return reject("storage", "无法打开本地数据库（演示）");
      }
      return tasks;
    },

    async addTask(input: AddTaskInput): Promise<TaskMutation> {
      await delay(700);
      const candidate = resolveMock(input.url, tasks);
      if ("kind" in candidate) return reject(candidate.kind, candidate.message);
      const existing = findDuplicates(candidate, tasks);
      if (existing.length > 0 && !input.force) {
        return { result: "duplicate", existing, candidate };
      }
      const task = snapshot(
        nextId++,
        {
          anchorName: candidate.anchorName,
          anchorId: candidate.anchorId ?? undefined,
          roomId: candidate.roomId,
          url: candidate.url,
          monitoring: "enabled",
          live: { state: "offline", checkedAt: Date.now() },
          record: { state: "idle" },
          quality: input.quality ?? settings.record_quality,
          notifyOnly: input.notifyOnly ?? settings.only_notify_no_record,
          note: input.note ?? "",
        },
        Date.now(),
      );
      task.liveUrl = candidate.liveUrl;
      tasks = [...tasks, task];
      updatedChannel.emit(task);
      log(task, "user_action", "添加任务");
      return { result: "added", task };
    },

    async updateTask(input) {
      const task = tasks.find((item) => item.id === input.id);
      if (!task) return reject("not_found", `任务 ${input.id} 不存在`);
      const patch: Partial<TaskSnapshot> = {};
      if (input.url !== undefined && input.url.trim() !== task.url) {
        if (isActive(task.record)) {
          return reject("recording_active", `${task.anchorName} 正在录制，停止录制后才能修改链接`);
        }
        await delay(500);
        const candidate = resolveMock(input.url, tasks);
        if ("kind" in candidate) return reject(candidate.kind, candidate.message);
        const existing = findDuplicates(candidate, tasks, task.id);
        if (existing.length > 0 && !input.force) return { result: "duplicate", existing, candidate };
        Object.assign(patch, {
          url: candidate.url,
          roomId: candidate.roomId,
          anchorId: candidate.anchorId,
          anchorName: candidate.anchorName,
          liveUrl: candidate.liveUrl,
          live: { state: "unknown" },
        });
      }
      if (input.quality !== undefined) patch.quality = input.quality;
      if (input.notifyOnly !== undefined) patch.notifyOnly = input.notifyOnly;
      if (input.note !== undefined) patch.note = input.note;
      const next = rederive(task, patch);
      replace(next);
      log(next, "user_action", "修改任务");
      return { result: "updated", task: next };
    },

    async deleteTasks(ids) {
      const busy = tasks.find((task) => ids.includes(task.id) && isActive(task.record));
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
        const recording = isActive(task.record);
        const next = rederive(task, {
          monitoring,
          live: recording ? task.live : enabled ? { state: "checking", since: Date.now() } : { state: "unknown" },
          // 与 Rust 一致：重新启用监控时清除录制失败。
          record: enabled && task.record.state === "failed" ? { state: "idle" } : task.record,
        });
        replace(next);
        log(next, "user_action", enabled ? "开始监控" : "暂停监控");
        result.push(next);
        if (enabled && !recording) {
          setTimeout(() => {
            const current = tasks.find((item) => item.id === task.id);
            if (current?.live.state === "checking") {
              patchTask(task.id, { live: { state: "offline", checkedAt: Date.now() } });
            }
          }, 1500);
        }
      }
      return result;
    },

    async stopRecording(ids) {
      if (!stopSupported) return reject("not_implemented", "录制功能尚未接入");
      for (const id of ids) {
        const task = tasks.find((item) => item.id === id);
        if (task) log(task, "user_action", "手动停止录制");
        finishRecording(id, "手动停止录制");
      }
      return tasks.filter((task) => ids.includes(task.id));
    },

    getSettings: () => Promise.resolve(settings),
    async updateSettings(values) {
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

    onFfmpegStatus: (handler) => ffmpegChannel.subscribe(handler),
    onTaskUpdated: (handler) => updatedChannel.subscribe(handler),
    onTaskRemoved: (handler) => removedChannel.subscribe(handler),
    onActivity: (handler) => activityChannel.subscribe(handler),
  };

  (window as unknown as { __lubobaoMock: unknown }).__lubobaoMock = {
    setFfmpeg: (name: string) => {
      clearInterval(simulation);
      const status = ffmpegPresets[name];
      if (status) setFfmpeg(status);
    },
    simulateDownload,
    goLive,
    endLive,
    setStopSupported: (value: boolean) => {
      stopSupported = value;
    },
    setEvents: (value: boolean) => {
      eventsEnabled = value;
    },
    backend,
  };

  return backend;
}
