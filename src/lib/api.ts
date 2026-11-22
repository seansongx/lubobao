// 前后端接口层：每个 Tauri 命令和事件的类型化封装。
// 类型与 Rust 的 serde 输出一一对应（src-tauri/src/tasks、src-tauri/src/ffmpeg/status.rs）。
// 不在 Tauri 环境里（npm run dev 在浏览器打开）时改用 mock-api.ts 的内存假数据。

import { invoke, isTauri } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

import { createMockBackend } from "@/lib/mock-api";

// ---- FFmpeg（src-tauri/src/ffmpeg/status.rs） ----

export type FfmpegStatus =
  | { state: "checking" }
  | { state: "notInstalled" }
  | { state: "waiting" }
  | { state: "downloading"; downloaded: number; total: number; source: string }
  | { state: "verifying" }
  | {
      state: "ready";
      path: string;
      ffmpegVersion: string;
      packageVersion: string;
      outdated: boolean;
      installError: string | null;
    }
  | { state: "failed"; reason: string; retryable: boolean };

// ---- 任务（src-tauri/src/tasks/model.rs） ----

export type TaskId = number;
export type Platform = "douyin";
export type Quality = "OD" | "UHD" | "HD" | "SD" | "LD";
export type Monitoring = "enabled" | "paused";
export type PlatformErrorKind =
  | "network"
  | "invalid_url"
  | "auth_required"
  | "rate_limited"
  | "parse"
  | "config";

/** 时间均为 Unix 毫秒。 */
export type LiveState =
  | { state: "unknown" }
  | { state: "checking"; since: number }
  | { state: "offline"; checkedAt: number }
  | { state: "live"; checkedAt: number; title: string | null }
  | {
      state: "checkFailed";
      kind: PlatformErrorKind;
      message: string;
      failedAt: number;
      nextRetryAt: number | null;
      attempt: number;
    };

export type RecordState =
  | { state: "idle" }
  | {
      state: "recording";
      filePath: string;
      startedAt: number;
      bytesWritten: number;
      /** kbit/s */
      bitrateKbps: number;
    }
  | { state: "finishing"; filePath: string; startedAt: number }
  | { state: "failed"; reason: string; filePath: string | null; failedAt: number };

/** 界面显示的七种状态，由 Rust 从监控意图、直播状态、录制状态推导。 */
export type DisplayStatus =
  | "recording"
  | "checking"
  | "waiting"
  | "retrying"
  | "notify"
  | "needsAction"
  | "paused";

export interface TaskSnapshot {
  id: TaskId;
  url: string;
  platform: Platform;
  platformName: string;
  roomId: string;
  anchorId: string | null;
  anchorName: string;
  liveUrl: string | null;
  quality: Quality;
  notifyOnly: boolean;
  note: string;
  createdAt: number;
  monitoring: Monitoring;
  live: LiveState;
  record: RecordState;
  /** 用户手动停止了录制：本次直播不再自动录制，下播或重新启用监控后恢复。此时 display 为 notify。 */
  recordHeld: boolean;
  display: DisplayStatus;
  displayReason: string | null;
}

export interface ResolvedRoom {
  url: string;
  platform: Platform;
  platformName: string;
  roomId: string;
  anchorId: string | null;
  anchorName: string;
  liveUrl: string | null;
  isLive: boolean;
  title: string | null;
}

export type TaskMutation =
  | { result: "added"; task: TaskSnapshot }
  | { result: "updated"; task: TaskSnapshot }
  | { result: "duplicate"; existing: TaskSnapshot[]; candidate: ResolvedRoom };

export interface AddTaskInput {
  url: string;
  /** 缺省用设置 record_quality。 */
  quality?: Quality;
  /** 缺省用设置 only_notify_no_record。 */
  notifyOnly?: boolean;
  note?: string;
  /** 用户确认重复后强制添加。 */
  force?: boolean;
}

export interface UpdateTaskInput {
  id: TaskId;
  url?: string;
  quality?: Quality;
  notifyOnly?: boolean;
  note?: string;
  force?: boolean;
}

// ---- 错误（src-tauri/src/tasks/mod.rs CommandError） ----

export type CommandErrorKind =
  | PlatformErrorKind
  | "unsupported_platform"
  | "not_found"
  | "invalid_input"
  | "recording_active"
  | "not_implemented"
  | "storage";

/** 命令失败时 Promise 以此对象 reject，message 可直接显示给用户。 */
export interface CommandError {
  kind: CommandErrorKind;
  message: string;
}

export function isCommandError(value: unknown): value is CommandError {
  return (
    typeof value === "object" &&
    value !== null &&
    typeof (value as CommandError).kind === "string" &&
    typeof (value as CommandError).message === "string"
  );
}

// ---- 动态（src-tauri/src/tasks/activity.rs） ----

export type ActivityKind =
  | "live_start"
  | "live_end"
  | "record_start"
  | "record_end"
  | "failure"
  /** 不影响录制、但用户应当知道的问题，例如代理协议不支持而改为直连。 */
  | "warning"
  | "user_action";

export interface ActivityEntry {
  id: number;
  at: number;
  taskId: TaskId | null;
  anchorName: string;
  kind: ActivityKind;
  message: string;
  detail: Record<string, unknown> | null;
}

export interface ActivityQuery {
  taskId?: TaskId;
  kinds?: ActivityKind[];
  /** 上一页的 nextBefore。 */
  before?: number;
  /** 默认 50，最大 200。 */
  limit?: number;
}

export interface ActivityPage {
  items: ActivityEntry[];
  nextBefore: number | null;
}

// ---- 设置（src-tauri/src/tasks/settings.rs，键名沿用 StreamCap） ----

/** 与 settings.rs 的 ENUM_SETTINGS 一致。 */
export type VideoFormat =
  | "TS"
  | "MP4"
  | "FLV"
  | "MKV"
  | "MOV"
  | "NUT"
  | "WAV"
  | "MP3"
  | "WMA"
  | "M4A"
  | "AAC";

export interface Settings {
  live_save_path: string;
  loop_time_seconds: number;
  platform_max_concurrent_requests: number;
  video_format: VideoFormat;
  record_quality: Quality;
  segmented_recording_enabled: boolean;
  video_segment_time: number;
  /** GB */
  recording_space_threshold: number;
  default_live_source: "FLV" | "HLS";
  force_https_recording: boolean;
  flv_use_direct_download: boolean;
  convert_to_mp4: boolean;
  delete_original: boolean;
  generate_time_subtitle_file: boolean;
  only_notify_no_record: boolean;
  filename_includes_title: boolean;
  remove_emojis: boolean;
  folder_name_platform: boolean;
  folder_name_author: boolean;
  folder_name_time: boolean;
  folder_name_title: boolean;
  enable_proxy: boolean;
  proxy_address: string;
  /**
   * 检测和录制走代理的平台键，逗号分隔（中文逗号也可以，空格忽略，区分大小写），
   * 例如 "tiktok, twitch"。默认与 StreamCap 相同，只有海外平台；不在列表里的平台直连。
   * 代理只在 enable_proxy 为 true 且 proxy_address 非空时生效。
   */
  default_platform_with_proxy: string;
  execute_custom_script: boolean;
  custom_script_command: string;
  /**
   * 平台 Cookie：键为平台键（"douyin"、"kuaishou"……），值为 Cookie 字符串，空字符串表示不用。
   * 对应 StreamCap cookies.json。更新时整个对象替换。值含换行等无法放进请求头的字符时，
   * updateSettings 以 kind "config" 拒绝。
   */
  platform_cookies: Record<string, string>;
  system_notification_enabled: boolean;
  stream_start_notification_enabled: boolean;
  stream_end_notification_enabled: boolean;
}

// ---- 调度（src-tauri/src/scheduler/mod.rs） ----

/** 状态栏用的调度概况，变化时通过 scheduler-status 事件推送。 */
export interface SchedulerStatus {
  /** 最近一次排定的检测（Unix 毫秒）；没有排定的检测时为 null（无监控任务，或都在检测、录制）。 */
  nextCheckAt: number | null;
  /** 正在请求平台的任务数。 */
  checking: number;
  /** 正在录制（含启动中、收尾中）的任务数。 */
  recording: number;
}

// ---- 后端接口 ----

export interface Backend {
  getFfmpegStatus(): Promise<FfmpegStatus>;
  retryFfmpegInstall(): Promise<FfmpegStatus>;
  listTasks(): Promise<TaskSnapshot[]>;
  addTask(input: AddTaskInput): Promise<TaskMutation>;
  updateTask(input: UpdateTaskInput): Promise<TaskMutation>;
  /** 返回实际删除的 id。任一任务正在录制时整批不删除。 */
  deleteTasks(ids: TaskId[]): Promise<TaskId[]>;
  /** 只改监控意图。 */
  setMonitoring(ids: TaskId[], enabled: boolean): Promise<TaskSnapshot[]>;
  /**
   * 手动停止录制，立即返回；录制状态随后经 finishing 回到 idle（task-updated 推送）。
   * 被停止的任务 recordHeld 为 true：本次直播不再自动录制。
   */
  stopRecording(ids: TaskId[]): Promise<TaskSnapshot[]>;
  getSettings(): Promise<Settings>;
  /** 部分更新，返回完整设置。任一项无效时整批不保存。 */
  updateSettings(values: Partial<Settings>): Promise<Settings>;
  listActivity(query?: ActivityQuery): Promise<ActivityPage>;
  getSchedulerStatus(): Promise<SchedulerStatus>;

  onFfmpegStatus(handler: (status: FfmpegStatus) => void): Promise<UnlistenFn>;
  onTaskUpdated(handler: (task: TaskSnapshot) => void): Promise<UnlistenFn>;
  onTaskRemoved(handler: (payload: { ids: TaskId[] }) => void): Promise<UnlistenFn>;
  onActivity(handler: (entry: ActivityEntry) => void): Promise<UnlistenFn>;
  onSchedulerStatus(handler: (status: SchedulerStatus) => void): Promise<UnlistenFn>;
}

function on<T>(event: string, handler: (payload: T) => void): Promise<UnlistenFn> {
  return listen<T>(event, (message) => handler(message.payload));
}

const tauriBackend: Backend = {
  getFfmpegStatus: () => invoke("get_ffmpeg_status"),
  retryFfmpegInstall: () => invoke("retry_ffmpeg_install"),
  listTasks: () => invoke("list_tasks"),
  addTask: (input) => invoke("add_task", { input }),
  updateTask: (input) => invoke("update_task", { input }),
  deleteTasks: (ids) => invoke("delete_tasks", { ids }),
  setMonitoring: (ids, enabled) => invoke("set_monitoring", { ids, enabled }),
  stopRecording: (ids) => invoke("stop_recording", { ids }),
  getSettings: () => invoke("get_settings"),
  updateSettings: (values) => invoke("update_settings", { values }),
  listActivity: (query) => invoke("list_activity", { query: query ?? null }),
  getSchedulerStatus: () => invoke("get_scheduler_status"),

  onFfmpegStatus: (handler) => on("ffmpeg-status", handler),
  onTaskUpdated: (handler) => on("task-updated", handler),
  onTaskRemoved: (handler) => on("task-removed", handler),
  onActivity: (handler) => on("activity", handler),
  onSchedulerStatus: (handler) => on("scheduler-status", handler),
};

/** 是否运行在浏览器假数据模式。 */
export const isMockBackend = !isTauri();

export const api: Backend = isMockBackend ? createMockBackend() : tauriBackend;
