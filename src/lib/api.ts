// 前后端接口层：每个 Tauri 命令和事件的类型化封装。
// 类型与 Rust 的 serde 输出一一对应（src-tauri/src/tasks、src-tauri/src/ffmpeg/status.rs）。
// 不在 Tauri 环境里（npm run dev 在浏览器打开）时改用 mock-api.ts 的内存假数据。

import { invoke, isTauri } from "@tauri-apps/api/core";
import { getVersion } from "@tauri-apps/api/app";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { join, videoDir } from "@tauri-apps/api/path";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { openPath as openerOpenPath, openUrl as openerOpenUrl } from "@tauri-apps/plugin-opener";

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
export type Platform = "douyin" | "kuaishou";
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
  execute_custom_script: boolean;
  custom_script_command: string;
  system_notification_enabled: boolean;
  stream_start_notification_enabled: boolean;
  stream_end_notification_enabled: boolean;
  /** 逗号分隔的平台键，只有列出的平台走代理。默认值沿用 StreamCap，只有海外平台。 */
  default_platform_with_proxy: string;
  /** 平台键（douyin、kuaishou）到 Cookie 字符串。 */
  platform_cookies: Partial<Record<Platform, string>>;
}

/**
 * 后端默认值（settings.rs 的 defaults），用于“恢复默认值”。保存目录依赖系统，
 * 由 Backend.defaultSaveDir 提供。
 */
export const settingDefaults: Omit<Settings, "live_save_path"> = {
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
  default_platform_with_proxy:
    "tiktok, sooplive, pandalive, winktv, flextv, popkontv, twitch, liveme, showroom, chzzk, shopee, shp, youtu, youtube, lang",
  platform_cookies: {},
};

// ---- 后端接口 ----

export interface Backend {
  getFfmpegStatus(): Promise<FfmpegStatus>;
  retryFfmpegInstall(): Promise<FfmpegStatus>;
  /** 忽略校验缓存重新计算哈希，不通过时重新安装。返回调用时的状态，后续变化走事件。 */
  reverifyFfmpeg(): Promise<FfmpegStatus>;
  listTasks(): Promise<TaskSnapshot[]>;
  addTask(input: AddTaskInput): Promise<TaskMutation>;
  updateTask(input: UpdateTaskInput): Promise<TaskMutation>;
  /** 返回实际删除的 id。任一任务正在录制时整批不删除。 */
  deleteTasks(ids: TaskId[]): Promise<TaskId[]>;
  /** 只改监控意图。 */
  setMonitoring(ids: TaskId[], enabled: boolean): Promise<TaskSnapshot[]>;
  /** 录制引擎接入前返回 not_implemented。 */
  stopRecording(ids: TaskId[]): Promise<TaskSnapshot[]>;
  getSettings(): Promise<Settings>;
  /** 部分更新，返回完整设置。任一项无效时整批不保存。 */
  updateSettings(values: Partial<Settings>): Promise<Settings>;
  listActivity(query?: ActivityQuery): Promise<ActivityPage>;

  /** 默认保存目录：系统「视频」目录下的「录播宝」，与 tasks/mod.rs 的 default_save_dir 一致。 */
  defaultSaveDir(): Promise<string>;
  /** 应用版本号（tauri.conf.json 的 version）。 */
  getAppVersion(): Promise<string>;
  /** 弹出系统目录选择框，取消时返回 null。 */
  pickDirectory(options?: { title?: string; defaultPath?: string }): Promise<string | null>;
  /** 用系统默认程序打开文件或目录。 */
  openPath(path: string): Promise<void>;
  /** 用系统浏览器打开链接。 */
  openUrl(url: string): Promise<void>;

  onFfmpegStatus(handler: (status: FfmpegStatus) => void): Promise<UnlistenFn>;
  onTaskUpdated(handler: (task: TaskSnapshot) => void): Promise<UnlistenFn>;
  onTaskRemoved(handler: (payload: { ids: TaskId[] }) => void): Promise<UnlistenFn>;
  onActivity(handler: (entry: ActivityEntry) => void): Promise<UnlistenFn>;
}

function on<T>(event: string, handler: (payload: T) => void): Promise<UnlistenFn> {
  return listen<T>(event, (message) => handler(message.payload));
}

const tauriBackend: Backend = {
  getFfmpegStatus: () => invoke("get_ffmpeg_status"),
  retryFfmpegInstall: () => invoke("retry_ffmpeg_install"),
  reverifyFfmpeg: () => invoke("reverify_ffmpeg"),
  listTasks: () => invoke("list_tasks"),
  addTask: (input) => invoke("add_task", { input }),
  updateTask: (input) => invoke("update_task", { input }),
  deleteTasks: (ids) => invoke("delete_tasks", { ids }),
  setMonitoring: (ids, enabled) => invoke("set_monitoring", { ids, enabled }),
  stopRecording: (ids) => invoke("stop_recording", { ids }),
  getSettings: () => invoke("get_settings"),
  updateSettings: (values) => invoke("update_settings", { values }),
  listActivity: (query) => invoke("list_activity", { query: query ?? null }),

  defaultSaveDir: async () => join(await videoDir(), "录播宝"),
  getAppVersion: () => getVersion(),
  async pickDirectory(options) {
    const picked = await openDialog({ directory: true, multiple: false, ...options });
    return typeof picked === "string" ? picked : null;
  },
  openPath: (path) => openerOpenPath(path),
  openUrl: (url) => openerOpenUrl(url),

  onFfmpegStatus: (handler) => on("ffmpeg-status", handler),
  onTaskUpdated: (handler) => on("task-updated", handler),
  onTaskRemoved: (handler) => on("task-removed", handler),
  onActivity: (handler) => on("activity", handler),
};

/** 是否运行在浏览器假数据模式。 */
export const isMockBackend = !isTauri();

export const api: Backend = isMockBackend ? createMockBackend() : tauriBackend;
