// 浏览器假数据（npm run dev，不在 Tauri 里时使用），方便调界面。
// 任务覆盖七种显示状态；FFmpeg 状态可用地址参数切换：
//   ?ffmpeg=checking | notInstalled | waiting | downloading | verifying | ready | updating | outdated | failed | fatal
// 缺省时模拟一次下载：下载中 → 校验 → 可用。控制台可调用 window.__lubobaoMock.setFfmpeg("failed")。
// 显示状态的推导复刻 src-tauri/src/tasks/model.rs 的 derive_display，只用于假数据。

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
  Partial<Pick<TaskSnapshot, "notifyOnly" | "quality" | "note">>;

function snapshot(id: TaskId, seed: MockSeed, createdAt: number): TaskSnapshot {
  const notifyOnly = seed.notifyOnly ?? false;
  const display = deriveDisplay(seed.monitoring, seed.live, seed.record, notifyOnly);
  return {
    id,
    url: `https://live.douyin.com/${seed.roomId}`,
    platform: "douyin",
    platformName: "抖音",
    roomId: seed.roomId,
    anchorId: `MS4wLjABAAAA-mock-${id}`,
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
    next.display = deriveDisplay(next.monitoring, next.live, next.record, next.notifyOnly);
    next.displayReason = displayReason(next.display, next.live, next.record);
    return next;
  }

  for (const task of tasks.slice().reverse()) log(task, "user_action", "添加任务");

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
        const recording = task.record.state === "recording" || task.record.state === "finishing";
        const next = rederive(task, {
          monitoring,
          live: recording ? task.live : { state: "unknown" },
          // 与 Rust 一致：重新启用监控时清除录制失败。
          record: enabled && task.record.state === "failed" ? { state: "idle" } : task.record,
        });
        replace(next);
        log(next, "user_action", enabled ? "开始监控" : "暂停监控");
        result.push(next);
      }
      return result;
    },

    stopRecording: () => reject("not_implemented", "录制功能尚未接入"),

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
    backend,
  };

  return backend;
}
