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
}

// ---- 录像库（src-tauri/src/library） ----

export type LibraryErrorKind =
  | "root_missing"
  | "root_not_directory"
  | "permission_denied"
  | "not_found"
  | "invalid_path"
  | "invalid_target"
  | "open_failed"
  | "io";

/** 录像库命令失败时 Promise 以此对象 reject；root 为录像根目录的绝对路径。 */
export interface LibraryError {
  kind: LibraryErrorKind;
  message: string;
  root: string;
}

export function isLibraryError(value: unknown): value is LibraryError {
  return isCommandError(value) && typeof (value as unknown as LibraryError).root === "string";
}

export interface LibraryEntry {
  name: string;
  /** 相对于录像根目录，`/` 分隔；传回 openLibraryFile 等命令。 */
  path: string;
  kind: "dir" | "file";
  /** 字节，目录为 null。 */
  size: number | null;
  /** Unix 毫秒。 */
  modifiedAt: number | null;
  /** 目录下的子目录和录像文件数。 */
  childCount: number | null;
  /** 分段序号（`_000`）。 */
  segment: number | null;
  /** 同一次录制的分组键。 */
  group: string | null;
}

export interface LibraryListing {
  root: string;
  /** 当前目录，根目录为 ""。 */
  path: string;
  /** 目录在前，文件按时间倒序，分段按序号。 */
  entries: LibraryEntry[];
}

/** 根目录所在磁盘的空间；根目录不存在时取最近的已存在上级目录。很快，可以定时查询。 */
export interface DiskSpace {
  root: string;
  rootExists: boolean;
  availableBytes: number | null;
  totalBytes: number | null;
}

export interface LibraryUsage {
  usedBytes: number;
  fileCount: number;
}

/** 根目录占用。Rust 缓存结果，同一时间只遍历一次；computing 时 usage 是上次的结果或 null。 */
export interface UsageSnapshot {
  root: string;
  rootExists: boolean;
  usage: LibraryUsage | null;
  /** Unix 毫秒。 */
  computedAt: number | null;
  computing: boolean;
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
  /** 录制引擎接入前返回 not_implemented。 */
  stopRecording(ids: TaskId[]): Promise<TaskSnapshot[]>;
  getSettings(): Promise<Settings>;
  /** 部分更新，返回完整设置。任一项无效时整批不保存。 */
  updateSettings(values: Partial<Settings>): Promise<Settings>;
  listActivity(query?: ActivityQuery): Promise<ActivityPage>;
  /** 列出录像根目录下的子目录；path 缺省为根目录。 */
  listLibrary(path?: string): Promise<LibraryListing>;
  getDiskSpace(): Promise<DiskSpace>;
  /** refresh 为 true 时忽略缓存有效期（30 秒）。最多等待 2 秒，仍在统计时 computing 为 true。 */
  getLibraryUsage(refresh?: boolean): Promise<UsageSnapshot>;
  /** 用系统默认播放器打开录像文件。 */
  openLibraryFile(path: string): Promise<void>;
  /** 在系统文件管理器中打开文件夹本身；path 为 "" 时打开根目录。 */
  openLibraryFolder(path: string): Promise<void>;
  /** 在系统文件管理器中显示并选中；path 为 "" 时打开根目录。 */
  revealLibraryItem(path: string): Promise<void>;

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
  listTasks: () => invoke("list_tasks"),
  addTask: (input) => invoke("add_task", { input }),
  updateTask: (input) => invoke("update_task", { input }),
  deleteTasks: (ids) => invoke("delete_tasks", { ids }),
  setMonitoring: (ids, enabled) => invoke("set_monitoring", { ids, enabled }),
  stopRecording: (ids) => invoke("stop_recording", { ids }),
  getSettings: () => invoke("get_settings"),
  updateSettings: (values) => invoke("update_settings", { values }),
  listActivity: (query) => invoke("list_activity", { query: query ?? null }),
  listLibrary: (path) => invoke("list_library", { path: path ?? null }),
  getDiskSpace: () => invoke("get_disk_space"),
  getLibraryUsage: (refresh) => invoke("get_library_usage", { refresh: refresh ?? false }),
  openLibraryFile: (path) => invoke("open_library_file", { path }),
  openLibraryFolder: (path) => invoke("open_library_folder", { path }),
  revealLibraryItem: (path) => invoke("reveal_library_item", { path }),

  onFfmpegStatus: (handler) => on("ffmpeg-status", handler),
  onTaskUpdated: (handler) => on("task-updated", handler),
  onTaskRemoved: (handler) => on("task-removed", handler),
  onActivity: (handler) => on("activity", handler),
};

/** 是否运行在浏览器假数据模式。 */
export const isMockBackend = !isTauri();

export const api: Backend = isMockBackend ? createMockBackend() : tauriBackend;
