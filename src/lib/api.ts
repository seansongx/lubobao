// 前后端接口层：每个 Tauri 命令和事件的类型化封装。
// 类型与 Rust 的 serde 输出一一对应（src-tauri/src/tasks、src-tauri/src/ffmpeg/status.rs）。
// 不在 Tauri 环境里（npm run dev 在浏览器打开）时改用 mock-api.ts 的内存假数据。

import { invoke, isTauri } from "@tauri-apps/api/core";
import { getVersion } from "@tauri-apps/api/app";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { openUrl as openerOpenUrl } from "@tauri-apps/plugin-opener";

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
export type Platform = "douyin" | "kuaishou" | "bilibili" | "huya" | "douyu" | "xiaohongshu" | "yy" | "netease" | "weibo" | "acfun" | "huajiao" | "inke" | "kugou" | "6room" | "baidu" | "zhihu" | "maoerfm";
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
  | "user_action"
  /** 录后转 MP4 的开始和完成。转换失败记为 failure、未转换或中止记为 warning，二者 detail.kind 为 "convert"；按 convert 筛选时一并返回。 */
  | "convert";

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
   * 平台 Cookie：键为平台键（"douyin"、"kuaishou"、"bilibili"、"huya"、"douyu"、"xiaohongshu"、"yy"、"netease"、"weibo"、"acfun"、"huajiao"、"inke"、"kugou"、"6room"、"baidu"、"zhihu"、"maoerfm"……），值为 Cookie 字符串，空字符串表示不用。
   * 对应 StreamCap cookies.json。更新时整个对象替换。值含换行等无法放进请求头的字符时，
   * updateSettings 以 kind "config" 拒绝。
   */
  platform_cookies: Record<string, string>;
  system_notification_enabled: boolean;
  stream_start_notification_enabled: boolean;
  stream_end_notification_enabled: boolean;
  /** 关闭主窗口时：ask 每次询问（默认），tray 隐藏到托盘，quit 退出。托盘不可用时一律退出。 */
  close_window_action: CloseWindowAction;
  /** 启动约 10 秒后自动检查一次更新（默认开）。 */
  auto_check_update: boolean;
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

// ---- 退出（src-tauri/src/desktop/quit.rs） ----

/**
 * idle：没有退出请求；confirming：有录制时从退出入口（托盘「退出」、macOS 菜单「退出录播宝」
 * Cmd+Q、关窗时选择退出）发起，等用户确认；Dock「退出」、注销关机不经确认，后台限时收尾；
 * finishing：已确认，正在停止录制、等文件收尾，随后应用退出。
 */
export type QuitPhase = "idle" | "confirming" | "finishing";

export type CloseWindowAction = "ask" | "tray" | "quit";

/**
 * 设置在设置页以外被修改（例如关窗时「记住我的选择」）后，在 window 上派发这个事件，
 * 设置页据此重新读取。
 */
export const SETTINGS_CHANGED_EVENT = "lubobao:settings-changed";

export function announceSettingsChanged() {
  window.dispatchEvent(new Event(SETTINGS_CHANGED_EVENT));
}

/** 有录制时点托盘「全部暂停监控」，pause-all-prompt 事件请前端确认。 */
export interface PauseAllPrompt {
  recording: number;
}

/** 收尾的原因：退出应用，或安装更新（收尾后安装并重启）。 */
export type QuitPurpose = "quit" | "update";

/** 退出状态，变化时通过 quit-state 事件推送。 */
export interface QuitState {
  phase: QuitPhase;
  /** 正在录制（含启动中、收尾中）的任务数。 */
  recording: number;
  purpose: QuitPurpose;
}

// ---- 应用内更新（src-tauri/src/desktop/update.rs） ----

/** 官网，手动下载新版本用。 */
export const WEBSITE_URL = "https://lubobao.xiaofany.com";

/**
 * not_configured：这个构建没有更新公钥（开发构建）；network：连不上更新服务器或下载中断；
 * signature：签名校验失败，已拒绝安装；unsupported：系统或安装方式不支持；busy：正在检查、
 * 下载、安装或退出；not_ready：还没检查或下载；install：安装失败（此时录制已停止，需要重启）。
 */
export type UpdateErrorKind =
  | "not_configured"
  | "network"
  | "signature"
  | "unsupported"
  | "busy"
  | "not_ready"
  | "install";

/** 更新命令失败时 Promise 以此对象 reject。 */
export interface UpdateError {
  kind: UpdateErrorKind;
  message: string;
}

export interface UpdateInfo {
  version: string;
  currentVersion: string;
  /** 更新说明，纯文本或简单的 Markdown。 */
  notes: string | null;
  /** RFC 3339。 */
  pubDate: string | null;
  /** 服务端标记的重要更新：醒目提示，但不强制。 */
  mandatory: boolean;
}

/** auto：可以应用内下载安装；manual：只能到官网下载（Linux deb 等）。 */
export type UpdateInstallMode = "auto" | "manual";

export type UpdateCheck =
  | { status: "upToDate"; currentVersion: string }
  | { status: "available"; update: UpdateInfo; install: UpdateInstallMode };

/**
 * 下载进度，update-progress 事件。total 为 null 时服务器没给长度。fallback 为 true 时主线路
 * 下载失败、已改从备用线路（GitHub）下载同一版本，downloaded 从 0 重新计数。
 */
export interface UpdateProgress {
  downloaded: number;
  total: number | null;
  fallback: boolean;
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
  /** 忽略校验缓存重新计算哈希，不通过时重新安装。返回调用时的状态，后续变化走事件。 */
  reverifyFfmpeg(): Promise<FfmpegStatus>;
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
  /**
   * 重新开始录制：清除手动停止的保持状态和录制失败，立即检测一次，仍在播就开始录制。
   * 只对监控中、会录制、当前没有录制的任务生效，返回其中重新开始的任务。
   */
  startRecording(ids: TaskId[]): Promise<TaskSnapshot[]>;
  getSettings(): Promise<Settings>;
  /** 后端的默认值（settings.rs），“恢复默认值”用。保存目录是本机的默认目录。 */
  getSettingDefaults(): Promise<Settings>;
  /** 部分更新，返回完整设置。任一项无效时整批不保存。 */
  updateSettings(values: Partial<Settings>): Promise<Settings>;
  listActivity(query?: ActivityQuery): Promise<ActivityPage>;
  getSchedulerStatus(): Promise<SchedulerStatus>;
  getQuitState(): Promise<QuitState>;
  /** 确认退出：进入 finishing，停止全部录制、等文件收尾后退出。 */
  confirmQuit(): Promise<void>;
  /** 取消退出；已在收尾时不能取消，原样返回。 */
  cancelQuit(): Promise<QuitState>;
  /**
   * 关窗三选一的结果（取消时不调用）。remember 时写入 close_window_action；保存失败时所选操作
   * 照常执行，Promise 以保存失败的原因（字符串）reject。
   */
  resolveClose(choice: "tray" | "quit", remember: boolean): Promise<void>;
  /** 确认托盘「全部暂停监控」：暂停全部监控中的任务（同时停止录制）。 */
  confirmPauseAll(): Promise<void>;
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

  /** 检查更新。发现新版本时后端记下，供 downloadUpdate 使用。失败时以 UpdateError reject。 */
  checkUpdate(): Promise<UpdateCheck>;
  /** 下载并校验签名，进度走 onUpdateProgress。签名不符时以 kind "signature" reject。 */
  downloadUpdate(): Promise<void>;
  /**
   * 停止调度和全部录制、等文件收尾（quit-state 为 finishing、purpose 为 update），再安装并重启。
   * 有录制时调用前先确认。成功时应用随即退出；失败时以 kind "install" reject。
   */
  installUpdate(): Promise<void>;
  /** 安装失败后重启录播宝（录制已收尾时才允许，Rust 命令 restart_after_failed_update）。 */
  restartAfterFailedUpdate(): Promise<void>;
  onUpdateProgress(handler: (progress: UpdateProgress) => void): Promise<UnlistenFn>;

  /** 应用版本号（tauri.conf.json 的 version）。 */
  getAppVersion(): Promise<string>;
  /** 弹出系统目录选择框，取消时返回 null。 */
  pickDirectory(options?: { title?: string; defaultPath?: string }): Promise<string | null>;
  /** 用系统浏览器打开链接。 */
  openUrl(url: string): Promise<void>;

  onFfmpegStatus(handler: (status: FfmpegStatus) => void): Promise<UnlistenFn>;
  onTaskUpdated(handler: (task: TaskSnapshot) => void): Promise<UnlistenFn>;
  onTaskRemoved(handler: (payload: { ids: TaskId[] }) => void): Promise<UnlistenFn>;
  onActivity(handler: (entry: ActivityEntry) => void): Promise<UnlistenFn>;
  onSchedulerStatus(handler: (status: SchedulerStatus) => void): Promise<UnlistenFn>;
  onQuitState(handler: (state: QuitState) => void): Promise<UnlistenFn>;
  /** close_window_action 为 ask 时关闭主窗口，请前端弹出三选一。 */
  onClosePrompt(handler: () => void): Promise<UnlistenFn>;
  onPauseAllPrompt(handler: (prompt: PauseAllPrompt) => void): Promise<UnlistenFn>;
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
  startRecording: (ids) => invoke("start_recording", { ids }),
  getSettings: () => invoke("get_settings"),
  getSettingDefaults: () => invoke("get_setting_defaults"),
  updateSettings: (values) => invoke("update_settings", { values }),
  listActivity: (query) => invoke("list_activity", { query: query ?? null }),
  getSchedulerStatus: () => invoke("get_scheduler_status"),
  getQuitState: () => invoke("get_quit_state"),
  confirmQuit: () => invoke("confirm_quit"),
  cancelQuit: () => invoke("cancel_quit"),
  resolveClose: (choice, remember) => invoke("resolve_close", { choice, remember }),
  confirmPauseAll: () => invoke("confirm_pause_all"),
  listLibrary: (path) => invoke("list_library", { path: path ?? null }),
  getDiskSpace: () => invoke("get_disk_space"),
  getLibraryUsage: (refresh) => invoke("get_library_usage", { refresh: refresh ?? false }),
  openLibraryFile: (path) => invoke("open_library_file", { path }),
  openLibraryFolder: (path) => invoke("open_library_folder", { path }),
  revealLibraryItem: (path) => invoke("reveal_library_item", { path }),

  checkUpdate: () => invoke("check_update"),
  downloadUpdate: () => invoke("download_update"),
  installUpdate: () => invoke("install_update"),
  restartAfterFailedUpdate: () => invoke("restart_after_failed_update"),
  onUpdateProgress: (handler) => on("update-progress", handler),

  getAppVersion: () => getVersion(),
  async pickDirectory(options) {
    const picked = await openDialog({ directory: true, multiple: false, ...options });
    return typeof picked === "string" ? picked : null;
  },
  openUrl: (url) => openerOpenUrl(url),

  onFfmpegStatus: (handler) => on("ffmpeg-status", handler),
  onTaskUpdated: (handler) => on("task-updated", handler),
  onTaskRemoved: (handler) => on("task-removed", handler),
  onActivity: (handler) => on("activity", handler),
  onSchedulerStatus: (handler) => on("scheduler-status", handler),
  onQuitState: (handler) => on("quit-state", handler),
  onClosePrompt: (handler) => on("close-prompt", () => handler()),
  onPauseAllPrompt: (handler) => on("pause-all-prompt", handler),
};

/**
 * 开发构建的桌面窗口里，调试面板可用 ?mock=1 改用示例数据（见 src/dev/overrides.ts）。
 * 正式构建里恒为 false。
 */
export const isDevMockInTauri =
  import.meta.env.DEV && isTauri() && new URLSearchParams(location.search).get("mock") === "1";

/**
 * 是否使用假数据：开发构建的浏览器预览，或开发构建里调试面板切到了示例数据。
 * 正式构建里恒为 false（`import.meta.env.DEV` 在构建时替换为常量），示例数据后端随之从包里删掉。
 */
export const isMockBackend = import.meta.env.DEV && (!isTauri() || isDevMockInTauri);

/**
 * 桌面端用示例数据时，退出、关窗和托盘「全部暂停监控」仍交给真实后端：这些提示来自真实的窗口和托盘，
 * 关系到后台真实的录制，示例数据接不住。
 */
function withRealDesktopShell(mock: Backend): Backend {
  return {
    ...mock,
    getQuitState: tauriBackend.getQuitState,
    confirmQuit: tauriBackend.confirmQuit,
    cancelQuit: tauriBackend.cancelQuit,
    onQuitState: tauriBackend.onQuitState,
    resolveClose: tauriBackend.resolveClose,
    onClosePrompt: tauriBackend.onClosePrompt,
    confirmPauseAll: tauriBackend.confirmPauseAll,
    onPauseAllPrompt: tauriBackend.onPauseAllPrompt,
  };
}

// 条件里直接写 import.meta.env.DEV，正式构建时整支连同 mock-api.ts 一起被删掉。
export const api: Backend =
  import.meta.env.DEV && isMockBackend
    ? isDevMockInTauri
      ? withRealDesktopShell(createMockBackend())
      : createMockBackend()
    : tauriBackend;
