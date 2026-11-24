// 浏览器假数据（npm run dev，不在 Tauri 里时使用），方便调界面。
// 应用内更新：window.__lubobaoMock.setUpdate("none" | "mandatory" | "manual" | "network" | "fallback" | "signature" | "install")
// 切换检查和安装的结果。
// 桌面外壳：控制台调用 window.__lubobaoMock.requestQuit() 模拟托盘「退出」，requestClose()
// 模拟关闭主窗口（按 close_window_action），requestPauseAll() 模拟托盘「全部暂停监控」。
// 显示状态的推导复刻 src-tauri/src/tasks/model.rs 的 derive_display，只用于假数据。
//
// 数据
//   任务：抖音和快手，覆盖七种显示状态（含两个录制中、一个仅通知）。
//   动态：三天的历史记录，覆盖全部类型（含 warning），id 按时间递增，与 before 分页一致。
//   录像库：按平台 / 主播分目录的录像树，含分段、空目录和 800 个分段的大目录。
//
// 地址参数
//   ?ffmpeg=checking | notInstalled | waiting | downloading | verifying | ready | updating | outdated | failed | fatal
//       缺省时模拟一次下载：下载中 → 校验 → 可用。
//   ?library=missing   录像根目录不存在；把设置 live_save_path 改成其他路径也视为不存在。
//   ?tasks=slow        任务列表延迟 1.5 秒返回（演示加载中）。
//   ?saveDelay=1500    设置保存延迟（毫秒，默认 150），用来复现连续修改的先后顺序。
//   ?tasks=fail        前两次读取任务列表失败（状态栏和直播间各一次，演示失败和重试）。
//   ?tasks=empty       没有直播间（演示空状态）；示例动态照旧生成，都显示为已删除的任务。
//
// 添加任务的演示链接（见 resolveMock）
//   live.douyin.com/{数字或抖音号}      识别成功；同一主播或同一链接判为重复
//   v.douyin.com/CeiU5cbX              主页短链，与 live.douyin.com/745964462470 是同一主播
//   v.douyin.com/{其他}                识别为列表中第一个主播（演示重复）
//   www.douyin.com/user/{sec_uid}      主页链接
//   live.kuaishou.com/u/{id}           快手直播间
//   live.bilibili.com/{房间号}          B站直播间
//   www.huya.com/{房间号或字母标识}       虎牙直播间（字母标识演示为房间号 660000）
//   www.douyu.com/{房间号}，topic/…?rid={房间号}  斗鱼直播间
//   www.xiaohongshu.com/livestream/{房间号}  小红书直播间；xhslink.com/… 演示为房间号 570000000000000001，主页链接返回 invalid_url
//   www.yy.com/{频道号}[/{子频道号}]   YY 直播间
//   cc.163.com/{CC号}，ds.163.com/glive/?ccid={CC号}  网易CC直播间
//   weibo.com/l/wblive/p/show/{live_id}  微博直播间；主页 weibo.com/u/{uid} 返回 auth_required（需要登录 Cookie）
//   live.acfun.cn/live/{用户 id}       AcFun 直播间
//   live.baidu.com/…live.html?room_id={房间号}  百度直播间
//   www.zhihu.com/theater/{直播间号}   知乎直播间
//   fm.missevan.com/live/{房间号}       猫耳FM 直播间（音频）
//   路径含 login / timeout / busy / parse  分别返回 auth_required / network / rate_limited / parse
//   其他域名                            unsupported_platform；无法解析的文本为 invalid_url
//
// 事件推送
//   录制中的任务每秒增长大小；等待开播的任务每隔一段时间模拟一次检测；重试中的任务按 nextRetryAt 重试；
//   调度每 180 秒一轮，推送 scheduler-status。手动停止录制后 recordHeld 为 true，下播或重新启用监控后恢复。
//
// 控制台 window.__lubobaoMock
//   setFfmpeg("failed")、simulateDownload()           FFmpeg 状态
//   setLibraryMissing(true)                          录像根目录是否存在
//   pushActivity("warning")                          推送一条新动态
//   goLive(id)、endLive(id)                          模拟开播（自动录制）与下播
//   setStopSupported(false)                          停止录制返回 not_implemented
//   setEvents(false)                                 暂停进度和检测的模拟推送
//   backend                                          假后端对象

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
  PauseAllPrompt,
  Platform,
  QuitState,
  RecordState,
  ResolvedRoom,
  SchedulerStatus,
  Settings,
  TaskId,
  TaskMutation,
  TaskSnapshot,
  UpdateError,
  UpdateInfo,
  UpdateProgress,
} from "@/lib/api";

type MockUpdateMode = "available" | "mandatory" | "manual" | "none" | "network" | "fallback" | "signature" | "install";

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
  Partial<Pick<TaskSnapshot, "notifyOnly" | "quality" | "note" | "platform" | "anchorId" | "url" | "liveUrl" | "recordHeld">>;

const HELD_REASON = "已手动停止录制，下次开播时自动录制";

/** 与 Rust 的 EndCause 一致。 */
type MockEndCause = "manual" | "paused" | "shutdown" | "streamEnded" | "networkError" | "stopped";
const endCauseLabel: Record<MockEndCause, string> = {
  manual: "手动停止",
  paused: "暂停监控",
  shutdown: "应用退出",
  streamEnded: "直播结束或断流",
  networkError: "网络错误",
  stopped: "已停止",
};

const mockPlatforms: Record<Platform, { name: string; roomUrl: (roomId: string) => string }> = {
  douyin: { name: "抖音", roomUrl: (roomId) => `https://live.douyin.com/${roomId}` },
  kuaishou: { name: "快手直播", roomUrl: (roomId) => `https://live.kuaishou.com/u/${roomId}` },
  bilibili: { name: "哔哩哔哩", roomUrl: (roomId) => `https://live.bilibili.com/${roomId}` },
  huya: { name: "虎牙直播", roomUrl: (roomId) => `https://www.huya.com/${roomId}` },
  douyu: { name: "斗鱼直播", roomUrl: (roomId) => `https://www.douyu.com/${roomId}` },
  xiaohongshu: { name: "小红书", roomUrl: (roomId) => `https://www.xiaohongshu.com/livestream/${roomId}` },
  yy: { name: "YY直播", roomUrl: (roomId) => `https://www.yy.com/${roomId}/${roomId}` },
  netease: { name: "网易CC直播", roomUrl: (roomId) => `https://cc.163.com/${roomId}/` },
  weibo: { name: "微博直播", roomUrl: (roomId) => `https://weibo.com/l/wblive/p/show/${roomId}` },
  acfun: { name: "Acfun", roomUrl: (roomId) => `https://live.acfun.cn/live/${roomId}` },
  baidu: { name: "百度直播", roomUrl: (roomId) => `https://live.baidu.com/m/media/pclive/pchome/live.html?room_id=${roomId}` },
  zhihu: { name: "知乎直播", roomUrl: (roomId) => `https://www.zhihu.com/theater/${roomId}` },
  maoerfm: { name: "猫耳FM直播", roomUrl: (roomId) => `https://fm.missevan.com/live/${roomId}` },
};

function snapshot(id: TaskId, seed: MockSeed, createdAt: number): TaskSnapshot {
  const notifyOnly = seed.notifyOnly ?? false;
  const recordHeld = seed.recordHeld ?? false;
  const display = deriveDisplay(seed.monitoring, seed.live, seed.record, notifyOnly || recordHeld);
  const platform = seed.platform ?? "douyin";
  const roomUrl = mockPlatforms[platform].roomUrl(seed.roomId);
  return {
    id,
    url: seed.url ?? roomUrl,
    platform,
    platformName: mockPlatforms[platform].name,
    roomId: seed.roomId,
    anchorId:
      seed.anchorId ??
      (platform === "douyin" ? `MS4wLjABAAAA-mock-${id}` : String((platform === "douyu" ? 10_000_000 : 100_000_000) + id)),
    anchorName: seed.anchorName,
    liveUrl: seed.liveUrl === undefined ? roomUrl : seed.liveUrl,
    quality: seed.quality ?? "OD",
    notifyOnly,
    note: seed.note ?? "",
    createdAt,
    monitoring: seed.monitoring,
    live: seed.live,
    record: seed.record,
    recordHeld,
    display,
    displayReason:
      display === "notify" && recordHeld && !notifyOnly ? HELD_REASON : displayReason(display, seed.live, seed.record),
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
        filePath: "D:\\Records\\抖音\\一只小羊驼\\一只小羊驼_2026-11-22_20-14-03_000.ts",
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
        filePath: "D:\\Records\\快手直播\\峡谷练习生\\峡谷练习生_2026-11-22_21-02-10_000.ts",
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
      anchorName: "星海电台",
      roomId: "21452505",
      platform: "bilibili",
      monitoring: "enabled",
      live: { state: "live", checkedAt: now - 50_000, title: "深夜歌回" },
      record: {
        state: "recording",
        filePath: "D:\\Records\\哔哩哔哩\\星海电台\\星海电台_2026-11-22_22-30-41_000.ts",
        startedAt: now - 12 * minute,
        bytesWritten: 412_000_000,
        bitrateKbps: 4600,
      },
    },
    {
      anchorName: "像素工坊",
      roomId: "7734200",
      platform: "bilibili",
      monitoring: "enabled",
      quality: "UHD",
      live: { state: "offline", checkedAt: now - 4 * minute },
      record: { state: "idle" },
    },
    {
      anchorName: "绝地老炮",
      roomId: "520520",
      platform: "douyu",
      monitoring: "enabled",
      live: { state: "live", checkedAt: now - 30_000, title: "四排上分" },
      record: {
        state: "recording",
        filePath: "D:\\Records\\斗鱼直播\\绝地老炮\\绝地老炮_2026-11-22_20-40-12_000.ts",
        startedAt: now - 47 * minute,
        bytesWritten: 1_052_000_000,
        bitrateKbps: 3100,
      },
    },
    {
      anchorName: "赛事回放站",
      roomId: "288016",
      platform: "douyu",
      monitoring: "enabled",
      quality: "UHD",
      live: { state: "offline", checkedAt: now - 4 * minute },
      record: { state: "idle" },
    },
    {
      anchorName: "山野手作",
      roomId: "570555131224313846",
      anchorId: "5f0000000000000000000001",
      platform: "xiaohongshu",
      monitoring: "enabled",
      live: { state: "offline", checkedAt: now - 3 * minute },
      record: { state: "idle" },
    },
    {
      anchorName: "晚风舞蹈",
      roomId: "22490906",
      platform: "yy",
      monitoring: "enabled",
      live: { state: "offline", checkedAt: now - 3 * minute },
      record: { state: "idle" },
    },
    {
      anchorName: "足球联赛官方",
      roomId: "367592480",
      platform: "netease",
      monitoring: "enabled",
      quality: "HD",
      live: { state: "offline", checkedAt: now - 3 * minute },
      record: { state: "idle" },
    },
    {
      anchorName: "纺织时装周",
      roomId: "1022:2321325368700659433556",
      anchorId: "5509110125",
      platform: "weibo",
      monitoring: "enabled",
      live: { state: "offline", checkedAt: now - 3 * minute },
      record: { state: "idle" },
    },
    {
      anchorName: "绘画小站",
      roomId: "179922",
      anchorId: "179922",
      platform: "acfun",
      monitoring: "enabled",
      live: { state: "offline", checkedAt: now - 2 * minute },
      record: { state: "idle" },
    },
    {
      anchorName: "面部提升祝东升",
      roomId: "11690716886",
      platform: "baidu",
      monitoring: "enabled",
      live: { state: "offline", checkedAt: now - 2 * minute },
      record: { state: "idle" },
    },
    {
      anchorName: "中国新闻网",
      roomId: "55785",
      platform: "zhihu",
      monitoring: "enabled",
      live: { state: "offline", checkedAt: now - 2 * minute },
      record: { state: "idle" },
    },
    {
      anchorName: "李晚星_全能主播",
      roomId: "869138407",
      anchorId: "34312404",
      platform: "maoerfm",
      monitoring: "enabled",
      live: { state: "offline", checkedAt: now - 2 * minute },
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
      platform: "huya",
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
      anchorName: "夜跑日记",
      roomId: "6620418",
      monitoring: "enabled",
      live: { state: "live", checkedAt: now - 2 * minute, title: "江边十公里" },
      record: { state: "idle" },
      recordHeld: true,
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
      platform: "huya",
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
  stream_start_notification_enabled: true,
  stream_end_notification_enabled: false,
  close_window_action: "ask",
  auto_check_update: true,
};

/** 与 src-tauri/src/tasks/settings.rs 的 ENUM_SETTINGS 一致。 */
const enumSettings: Partial<Record<keyof Settings, readonly string[]>> = {
  record_quality: ["OD", "UHD", "HD", "SD", "LD"],
  video_format: ["TS", "MP4", "FLV", "MKV", "MOV", "NUT", "WAV", "MP3", "WMA", "M4A", "AAC"],
  default_live_source: ["FLV", "HLS"],
  close_window_action: ["ask", "tray", "quit"],
};

/** 复刻 settings.rs 的 validate：类型一致、数值为正、整数项必须为整数、枚举项取合法值。 */
function validateSetting(key: string, value: unknown): string | null {
  if (!(key in defaultSettings)) return `未知的设置项：${key}`;
  const fallback = defaultSettings[key as keyof Settings];
  const shown = JSON.stringify(value);
  if (typeof fallback === "number") {
    if (typeof value !== "number" || !Number.isFinite(value) || value <= 0) {
      return `设置项 ${key} 的值无效：${shown}`;
    }
    if (Number.isInteger(fallback) && key !== "recording_space_threshold" && !Number.isInteger(value)) {
      return `设置项 ${key} 需要正整数：${shown}`;
    }
  } else if (typeof fallback === "object") {
    const valid =
      typeof value === "object" &&
      value !== null &&
      !Array.isArray(value) &&
      Object.values(value).every((item) => typeof item === "string");
    if (!valid) return `设置项 ${key} 的值无效：${shown}`;
  } else if (typeof value !== typeof fallback) {
    return `设置项 ${key} 的值无效：${shown}`;
  }
  const allowed = enumSettings[key as keyof Settings];
  if (allowed && !allowed.includes(value as string)) {
    return `设置项 ${key} 只能是 ${allowed.join("、")} 之一：${shown}`;
  }
  return null;
}

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
    斗鱼直播: {
      绝地老炮: {
        ...segments("绝地老炮_2026-11-22_20-40-12", 2, 47, 760 * MB),
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
        message: "录制结束：直播结束或断流，共 4 个分段",
        detail: { filePath, segments: 4, endCause: "streamEnded", durationMs: 110 * minute },
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
  push(15, {
    task: byName("峡谷练习生"),
    kind: "warning",
    message: "代理是 SOCKS，录制不支持，本次直连录制",
    detail: { proxy: "socks5://127.0.0.1:1080" },
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

/** 模拟 platforms::resolve：认抖音、快手和B站、虎牙、斗鱼、小红书、YY、网易CC、微博、AcFun、百度、知乎、猫耳FM，按链接内容返回各类错误。 */
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
  const platform: Platform | null = host.endsWith("douyin.com")
    ? "douyin"
    : host.endsWith("kuaishou.com")
      ? "kuaishou"
      : host === "live.bilibili.com"
        ? "bilibili"
      : host === "huya.com" || host.endsWith(".huya.com")
        ? "huya"
        : host.endsWith("douyu.com")
          ? "douyu"
          : host === "xiaohongshu.com" || host === "www.xiaohongshu.com" || host === "xhslink.com"
            ? "xiaohongshu"
            : host === "yy.com" || host === "www.yy.com"
              ? "yy"
              : host.endsWith("cc.163.com") || host === "ds.163.com"
                ? "netease"
                : host === "weibo.com" || host === "www.weibo.com"
                  ? "weibo"
                  : host === "live.acfun.cn"
                    ? "acfun"
                    : host === "live.baidu.com"
                      ? "baidu"
                      : host === "zhihu.com" || host === "www.zhihu.com"
                        ? "zhihu"
                        : host === "fm.missevan.com"
                          ? "maoerfm"
                          : null;
  if (!platform) return { kind: "unsupported_platform", message: `暂不支持 ${host} 的直播间` };
  if (/login/i.test(path)) return { kind: "auth_required", message: "该直播间需要登录后观看，请填写 Cookie" };
  if (/timeout|network/i.test(path)) return { kind: "network", message: `连接 ${host} 超时` };
  if (/busy/i.test(path)) return { kind: "rate_limited", message: "当前服务繁忙（10011），请稍后再试" };
  if (/parse/i.test(path)) return { kind: "parse", message: "直播间页面缺少 roomStore 数据" };

  const base = { url, platform, platformName: mockPlatforms[platform].name, isLive: false, title: null };
  const known = (roomId: string) => tasks.find((task) => task.platform === platform && task.roomId === roomId);

  if (platform === "huya") {
    const match = /^\/([\w-]+)\/?$/.exec(path);
    if (!match) return { kind: "invalid_url", message: `不是可识别的虎牙直播链接: ${url}` };
    const roomId = /^\d+$/.test(match[1]) ? match[1] : "660000";
    const task = known(roomId);
    return {
      ...base,
      roomId,
      anchorId: task?.anchorId ?? `huya-${roomId}`,
      anchorName: task?.anchorName ?? `虎牙主播 ${roomId}`,
      liveUrl: mockPlatforms.huya.roomUrl(roomId),
    };
  }
  if (platform === "kuaishou") {
    const match = /^\/u\/([\w.-]+)\/?$/.exec(path);
    if (!match) return { kind: "invalid_url", message: `无法从链接中找到快手直播间：${url}` };
    const roomId = match[1];
    const task = known(roomId);
    return {
      ...base,
      roomId,
      anchorId: task?.anchorId ?? `ks-${roomId}`,
      anchorName: task?.anchorName ?? `快手主播 ${roomId}`,
      liveUrl: mockPlatforms.kuaishou.roomUrl(roomId),
    };
  }
  if (platform === "bilibili") {
    const match = /^\/(?:h5\/|blanc\/)?0*([1-9]\d*)\/?$/.exec(path);
    if (!match) return { kind: "invalid_url", message: `不是可识别的 B站直播间链接: ${url}` };
    const roomId = match[1];
    const task = known(roomId);
    return {
      ...base,
      roomId,
      anchorId: task?.anchorId ?? `uid-${roomId}`,
      anchorName: task?.anchorName ?? `B站主播 ${roomId}`,
      liveUrl: mockPlatforms.bilibili.roomUrl(roomId),
    };
  }
  if (platform === "douyu") {
    const match = /^\/(\d+)\/?$/.exec(path);
    const roomId = match?.[1] ?? parsed.searchParams.get("rid");
    if (!roomId || !/^\d+$/.test(roomId)) return { kind: "invalid_url", message: `无法从链接中找到斗鱼房间号：${url}` };
    const task = known(roomId);
    return {
      ...base,
      roomId,
      anchorId: task?.anchorId ?? `dy-${roomId}`,
      anchorName: task?.anchorName ?? `斗鱼主播 ${roomId}`,
      liveUrl: mockPlatforms.douyu.roomUrl(roomId),
    };
  }
  if (platform === "xiaohongshu") {
    if (/^\/user\/profile\//.test(path)) {
      return { kind: "invalid_url", message: "小红书主页链接无法判断是否开播，请在直播间里复制直播间链接或分享链接（每场直播的链接不同）" };
    }
    const match = /^\/livestream\/(?:[^/]+\/)*(\d+)\/?$/.exec(path);
    const roomId = host === "xhslink.com" && path.length > 1 ? "570000000000000001" : match?.[1];
    if (!roomId) return { kind: "invalid_url", message: `不是可识别的小红书直播链接: ${url}` };
    const task = known(roomId);
    return {
      ...base,
      roomId,
      anchorId: task?.anchorId ?? "5f0000000000000000000002",
      anchorName: task?.anchorName ?? `小红书主播 ${roomId.slice(-4)}`,
      liveUrl: mockPlatforms.xiaohongshu.roomUrl(roomId),
    };
  }
  if (platform === "yy") {
    const match = /^\/(\d+)(?:\/(\d+))?\/?$/.exec(path);
    if (!match) return { kind: "invalid_url", message: `不是可识别的 YY 直播链接: ${url}` };
    const roomId = match[1];
    const task = known(roomId);
    return {
      ...base,
      roomId,
      anchorId: task?.anchorId ?? `yy-${roomId}`,
      anchorName: task?.anchorName ?? `YY 主播 ${roomId}`,
      liveUrl: `https://www.yy.com/${roomId}${match[2] ? `/${match[2]}` : ""}`,
    };
  }
  if (platform === "netease") {
    const match = /^\/(?:cc\/)?(\d+)\/?$/.exec(path);
    const roomId = match?.[1] ?? parsed.searchParams.get("ccid");
    if (!roomId || !/^\d+$/.test(roomId)) return { kind: "invalid_url", message: `不是可识别的网易CC直播链接: ${url}` };
    const task = known(roomId);
    return {
      ...base,
      roomId,
      anchorId: task?.anchorId ?? `cc-${roomId}`,
      anchorName: task?.anchorName ?? `网易CC主播 ${roomId}`,
      liveUrl: mockPlatforms.netease.roomUrl(roomId),
    };
  }
  if (platform === "weibo") {
    if (/^\/u\/\d+\/?$/.test(path)) {
      return { kind: "auth_required", message: "微博主页链接需要登录：请在设置里填写微博 Cookie，或改用直播间链接" };
    }
    const match = /^\/l\/wblive\/p\/show\/(\d+(?::|%3A)\d+)\/?$/i.exec(path);
    if (!match) return { kind: "invalid_url", message: `不是可识别的微博直播链接: ${url}` };
    const roomId = match[1].replace(/%3A/i, ":");
    const task = known(roomId);
    return {
      ...base,
      roomId,
      anchorId: task?.anchorId ?? `weibo-${roomId.slice(-6)}`,
      anchorName: task?.anchorName ?? `微博主播 ${roomId.slice(-4)}`,
      liveUrl: mockPlatforms.weibo.roomUrl(roomId),
    };
  }
  if (platform === "acfun") {
    const match = /^\/live\/(\d+)\/?$/.exec(path);
    if (!match) return { kind: "invalid_url", message: `不是可识别的 AcFun 直播链接: ${url}` };
    const roomId = match[1];
    const task = known(roomId);
    return {
      ...base,
      roomId,
      anchorId: task?.anchorId ?? roomId,
      anchorName: task?.anchorName ?? `AcFun 主播 ${roomId}`,
      liveUrl: mockPlatforms.acfun.roomUrl(roomId),
    };
  }
  if (platform === "baidu") {
    const roomId = parsed.searchParams.get("room_id") ?? "";
    if (!/^\d+$/.test(roomId)) return { kind: "invalid_url", message: `不是可识别的百度直播链接: ${url}` };
    const task = known(roomId);
    return {
      ...base,
      roomId,
      anchorId: task?.anchorId ?? null,
      anchorName: task?.anchorName ?? `百度主播 ${roomId.slice(-4)}`,
      liveUrl: mockPlatforms.baidu.roomUrl(roomId),
    };
  }
  if (platform === "zhihu") {
    const match = /^[/]theater[/]([0-9]+)[/]?$/.exec(path);
    if (!match) return { kind: "invalid_url", message: `不是可识别的知乎直播链接: ${url}` };
    const roomId = match[1];
    const task = known(roomId);
    return {
      ...base,
      roomId,
      anchorId: task?.anchorId ?? null,
      anchorName: task?.anchorName ?? `知乎主播 ${roomId}`,
      liveUrl: mockPlatforms.zhihu.roomUrl(roomId),
    };
  }
  if (platform === "maoerfm") {
    const match = /^[/]live[/]([0-9]+)[/]?$/.exec(path);
    if (!match) return { kind: "invalid_url", message: `不是可识别的猫耳FM直播链接: ${url}` };
    const roomId = match[1];
    const task = known(roomId);
    return {
      ...base,
      roomId,
      anchorId: task?.anchorId ?? null,
      anchorName: task?.anchorName ?? `猫耳FM 主播 ${roomId.slice(-4)}`,
      liveUrl: mockPlatforms.maoerfm.roomUrl(roomId),
    };
  }
  if (host === "v.douyin.com") {
    const code = path.split("/").filter(Boolean)[0];
    if (!code) return { kind: "invalid_url", message: "短链缺少短码" };
    if (code === "CeiU5cbX") {
      return {
        ...base,
        roomId: DEMO_ROOM,
        anchorId: DEMO_ANCHOR,
        anchorName: `主播 ${DEMO_ROOM}`,
        liveUrl: mockPlatforms.douyin.roomUrl(DEMO_ROOM),
      };
    }
    const first = tasks.find((task) => task.platform === "douyin");
    if (!first) return { kind: "invalid_url", message: "短链已失效" };
    return { ...base, roomId: first.roomId, anchorId: first.anchorId, anchorName: first.anchorName, liveUrl: first.liveUrl };
  }
  const user = /^\/user\/([\w-]+)/.exec(path);
  if (user) {
    return { ...base, roomId: user[1].slice(0, 12), anchorId: user[1], anchorName: `主播 ${user[1].slice(0, 6)}`, liveUrl: null };
  }
  const room = /^\/([\w.-]+)\/?$/.exec(path);
  if (host === "live.douyin.com" && room) {
    const roomId = room[1];
    const task = known(roomId);
    return {
      ...base,
      roomId,
      anchorId: task?.anchorId ?? (roomId === DEMO_ROOM ? DEMO_ANCHOR : `MS4wLjABAAAA-mock-anchor-${roomId}`),
      anchorName: task?.anchorName ?? `主播 ${roomId}`,
      liveUrl: mockPlatforms.douyin.roomUrl(roomId),
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
  const tasksMode = new URLSearchParams(window.location.search).get("tasks");
  let tasks = tasksMode === "empty" ? [] : seedTasks(now);
  let nextId = tasks.length + 1;
  let settings = { ...defaultSettings };
  const saveDelay = Number(new URLSearchParams(window.location.search).get("saveDelay") ?? 150) || 150;
  const activity: ActivityEntry[] = [];
  let nextActivityId = 1;
  // 与 Rust 一致，停止录制默认可用；setStopSupported(false) 演示 not_implemented。
  let stopSupported = true;
  let eventsEnabled = true;
  // 状态栏和直播间各订阅一次，两次都失败才能稳定演示。
  let failuresLeft = tasksMode === "fail" ? 2 : 0;

  const ffmpegChannel = new Channel<FfmpegStatus>();
  const updatedChannel = new Channel<TaskSnapshot>();
  const removedChannel = new Channel<{ ids: TaskId[] }>();
  const activityChannel = new Channel<ActivityEntry>();
  const schedulerChannel = new Channel<SchedulerStatus>();
  const quitChannel = new Channel<QuitState>();
  const closeChannel = new Channel<void>();
  const pauseAllChannel = new Channel<PauseAllPrompt>();

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

  // 假退出：有录制时先确认；确认后收尾 3 秒，浏览器里无法退出，回到 idle。
  let quit: QuitState = { phase: "idle", recording: 0, purpose: "quit" };
  function setQuit(phase: QuitState["phase"], purpose: QuitState["purpose"] = "quit") {
    quit = { phase, recording: schedulerStatus().recording, purpose };
    quitChannel.emit(quit);
  }

  // 假更新：window.__lubobaoMock.setUpdate(mode) 切换检查结果。
  // available（默认）、mandatory、manual（deb）、none、network、signature（下载完校验失败）、install（安装失败）。
  let updateMode: MockUpdateMode = "available";
  let updateDownloaded = false;
  const updateProgressChannel = new Channel<UpdateProgress>();
  function mockUpdateInfo(): UpdateInfo {
    return {
      version: "0.1.1",
      currentVersion: "0.1.0",
      notes: ["## 新功能", "", "- 录像库支持按主播筛选", "- 断流后自动续录", "", "## 修复", "", "- 修复 **抖音** 部分直播间检测失败"].join("\n"),
      pubDate: new Date().toISOString(),
      mandatory: updateMode === "mandatory",
    };
  }
  function requestQuit() {
    if (quit.phase === "finishing") return;
    if (schedulerStatus().recording > 0) {
      setQuit("confirming");
    } else {
      console.info("[mock] 没有录制，直接退出");
    }
  }

  function performClose(action: "tray" | "quit") {
    if (action === "tray") console.info("[mock] 隐藏到托盘");
    else requestQuit();
  }
  function requestClose() {
    const action = settings.close_window_action;
    if (action === "ask") closeChannel.emit();
    else performClose(action);
  }
  function pauseAll() {
    const ids = tasks.filter((task) => task.monitoring === "enabled").map((task) => task.id);
    void backend.setMonitoring(ids, false);
  }
  function requestPauseAll() {
    const recording = schedulerStatus().recording;
    if (recording > 0) pauseAllChannel.emit({ recording });
    else pauseAll();
  }

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

  function log(
    task: TaskSnapshot | null,
    kind: ActivityEntry["kind"],
    message: string,
    detail: ActivityEntry["detail"] = null,
  ) {
    const entry: ActivityEntry = {
      id: nextActivityId++,
      at: Date.now(),
      taskId: task?.id ?? null,
      anchorName: task?.anchorName ?? "",
      kind,
      message,
      detail,
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

  function patchTask(id: TaskId, patch: Partial<TaskSnapshot>): TaskSnapshot | null {
    const task = tasks.find((item) => item.id === id);
    if (!task) return null;
    const next = rederive(task, patch);
    replace(next);
    return next;
  }

  /** 与 Rust 一致：分段录制时显示第一个分段 `_000.ts`。 */
  function recordingPath(task: TaskSnapshot, stamp: string) {
    const suffix = settings.segmented_recording_enabled ? "_000.ts" : ".ts";
    return `${settings.live_save_path}\\${task.platformName}\\${task.anchorName}\\${task.anchorName}_${stamp}${suffix}`;
  }

  function startRecording(task: TaskSnapshot) {
    const started = Date.now();
    const stamp = new Date(started).toISOString().slice(0, 19).replace("T", "_").replace(/:/g, "-");
    return patchTask(task.id, {
      record: {
        state: "recording",
        filePath: recordingPath(task, stamp),
        startedAt: started,
        bytesWritten: 0,
        bitrateKbps: 4000 + Math.round(Math.random() * 3000),
      },
    });
  }

  /** 收尾 1.5 秒后回到空闲，写一条 record_end。 */
  function finishRecording(id: TaskId, cause: MockEndCause, patch: Partial<TaskSnapshot> = {}) {
    const task = tasks.find((item) => item.id === id);
    if (!task || task.record.state !== "recording") return task ?? null;
    const { filePath, startedAt } = task.record;
    const finishing = patchTask(id, { ...patch, record: { state: "finishing", filePath, startedAt } });
    setTimeout(() => {
      const current = tasks.find((item) => item.id === id);
      if (current?.record.state !== "finishing") return;
      const idle = patchTask(id, { record: { state: "idle" } });
      if (idle) {
        log(idle, "record_end", `录制结束：${endCauseLabel[cause]}`, {
          filePath,
          files: [filePath],
          segments: 1,
          endCause: cause,
          durationMs: Date.now() - startedAt,
        });
        if (settings.convert_to_mp4) {
          log(idle, "convert", "开始转为 MP4");
          setTimeout(() => log(idle, "convert", "已转为 MP4"), 1000);
        }
      }
      schedulerChannel.emit(schedulerStatus());
    }, 1500);
    schedulerChannel.emit(schedulerStatus());
    return finishing;
  }

  function goLive(id: TaskId) {
    const task = tasks.find((item) => item.id === id);
    if (!task || task.monitoring === "paused") return;
    const next = patchTask(id, { live: { state: "live", checkedAt: Date.now(), title: "演示直播" } });
    if (!next) return;
    log(next, "live_start", "检测到开播：演示直播");
    if (!next.notifyOnly && !next.recordHeld && !isActive(next.record)) {
      const recording = startRecording(next);
      if (recording) log(recording, "record_start", `开始录制 · ${qualityNames[recording.quality]}`);
      schedulerChannel.emit(schedulerStatus());
    }
  }

  function endLive(id: TaskId) {
    const task = tasks.find((item) => item.id === id);
    if (!task) return;
    finishRecording(id, "streamEnded");
    // 与 Rust 一致：下播后清除手动停止。
    const next = patchTask(id, { live: { state: "offline", checkedAt: Date.now() }, recordHeld: false });
    if (next && task.live.state === "live") log(next, "live_end", "已下播");
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
    reverifyFfmpeg: () => {
      const before = ffmpeg;
      if (before.state === "failed") {
        simulateDownload();
      } else if (before.state === "ready") {
        // 与后端一致：校验中 → 可用；旧版本加安装失败时重新安装仍失败，回到原状态。
        clearInterval(simulation);
        setFfmpeg({ state: "verifying" });
        setTimeout(() => setFfmpeg(before), 1500);
      }
      return Promise.resolve(before);
    },
    async listTasks() {
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
          platform: candidate.platform,
          url: candidate.url,
          liveUrl: candidate.liveUrl,
          monitoring: "enabled",
          live: { state: "offline", checkedAt: Date.now() },
          record: { state: "idle" },
          quality: input.quality ?? settings.record_quality,
          notifyOnly: input.notifyOnly ?? settings.only_notify_no_record,
          note: input.note ?? "",
        },
        Date.now(),
      );
      tasks = [...tasks, task];
      updatedChannel.emit(task);
      log(task, "user_action", "添加任务");
      schedulerChannel.emit(schedulerStatus());
      return { result: "added", task };
    },

    async updateTask(input) {
      const task = tasks.find((item) => item.id === input.id);
      if (!task) return reject("not_found", `任务 ${input.id} 不存在`);
      const patch: Partial<TaskSnapshot> = {};
      if (input.url !== undefined && input.url.trim() !== task.url) {
        // 与 Rust 一致：正在录制时不能改链接。
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
          platform: candidate.platform,
          platformName: candidate.platformName,
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
          live: enabled ? { state: "checking", since: Date.now() } : { state: "unknown" },
          record,
          recordHeld: enabled ? false : task.recordHeld,
        });
        replace(next);
        log(next, "user_action", enabled ? "开始监控" : "暂停监控");
        if (!enabled && recording) log(next, "record_end", "录制结束：暂停监控", { endCause: "paused" });
        result.push(next);
        if (enabled) {
          // 模拟一次检测：1.5 秒后回到未开播。
          setTimeout(() => {
            const current = tasks.find((item) => item.id === task.id);
            if (current?.live.state === "checking") patchTask(task.id, { live: { state: "offline", checkedAt: Date.now() } });
          }, 1500);
        }
      }
      schedulerChannel.emit(schedulerStatus());
      return result;
    },

    async stopRecording(ids) {
      if (!stopSupported) return reject("not_implemented", "录制功能尚未接入");
      const result: TaskSnapshot[] = [];
      for (const id of ids) {
        const task = tasks.find((item) => item.id === id);
        if (!task) continue;
        if (task.record.state !== "recording") {
          result.push(task);
          continue;
        }
        // 与 Rust 一致：先进入收尾中，ffmpeg 写完文件尾后回到空闲；本场直播不再自动录制。
        // 「录制结束：手动停止」写明是用户停止的。
        const finishing = finishRecording(id, "manual", { recordHeld: true });
        if (finishing) result.push(finishing);
      }
      return result;
    },

    async startRecording(ids) {
      if (!stopSupported) return reject("not_implemented", "录制功能尚未接入");
      const result: TaskSnapshot[] = [];
      for (const task of tasks.filter((item) => ids.includes(item.id))) {
        // 与 Rust 一致：只对监控中、会录制、当前没有录制的任务生效。
        if (task.monitoring !== "enabled" || task.notifyOnly || isActive(task.record)) continue;
        const next = rederive(task, {
          recordHeld: false,
          record: task.record.state === "failed" ? { state: "idle" } : task.record,
          live: { state: "checking", since: Date.now() },
        });
        replace(next);
        log(next, "user_action", "重新开始录制");
        result.push(next);
        // 模拟立即检测：之前在播的仍在播，开始录制；其余回到未开播。
        const wasLive = task.live.state === "live";
        setTimeout(() => {
          const current = tasks.find((item) => item.id === task.id);
          if (current?.live.state !== "checking") return;
          if (wasLive) goLive(task.id);
          else patchTask(task.id, { live: { state: "offline", checkedAt: Date.now() } });
        }, 1200);
      }
      schedulerChannel.emit(schedulerStatus());
      return result;
    },

    getSettings: () => Promise.resolve(settings),
    getSettingDefaults: () => Promise.resolve({ ...defaultSettings }),
    async updateSettings(values) {
      await delay(saveDelay);
      for (const [key, value] of Object.entries(values)) {
        const message = validateSetting(key, value);
        if (message) return reject("config", message);
      }
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
          (!query.kinds?.length ||
            query.kinds.includes(entry.kind) ||
            // 与 Rust 一致：「转为 MP4」也包括转换的失败和提醒。
            (query.kinds.includes("convert") && entry.detail?.kind === "convert")),
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

    async checkUpdate() {
      await delay(800);
      if (updateMode === "network") throw { kind: "network", message: "无法连接更新服务器：连接超时" } satisfies UpdateError;
      if (updateMode === "none") return { status: "upToDate", currentVersion: "0.1.0" };
      return { status: "available", update: mockUpdateInfo(), install: updateMode === "manual" ? "manual" : "auto" };
    },
    async downloadUpdate() {
      if (updateDownloaded) return;
      const total = 4_200_000;
      // fallback：主线路下到一半中断，改从备用线路重新下载。
      let fallback = false;
      for (let downloaded = 0; downloaded < total; ) {
        await delay(120);
        downloaded = Math.min(total, downloaded + 180_000);
        if (updateMode === "fallback" && !fallback && downloaded > total / 2) {
          fallback = true;
          downloaded = 0;
          updateProgressChannel.emit({ downloaded, total: null, fallback });
          await delay(800);
          continue;
        }
        updateProgressChannel.emit({ downloaded, total, fallback });
      }
      if (updateMode === "signature") {
        throw { kind: "signature", message: "更新包签名校验失败，已拒绝安装：Invalid signature" } satisfies UpdateError;
      }
      updateDownloaded = true;
    },
    async installUpdate() {
      if (!updateDownloaded) throw { kind: "not_ready", message: "请先下载更新" } satisfies UpdateError;
      updateDownloaded = false;
      setQuit("finishing", "update");
      await delay(2500);
      setQuit("idle");
      if (updateMode === "install") {
        throw { kind: "install", message: "安装失败：拒绝访问。 (os error 5)" } satisfies UpdateError;
      }
      console.info("[mock] 已安装更新并重启（浏览器里回到 idle）");
    },
    restartAfterFailedUpdate: async () => window.location.reload(),
    onUpdateProgress: (handler) => updateProgressChannel.subscribe(handler),

    getAppVersion: () => Promise.resolve("0.1.0"),
    pickDirectory: async (options) => {
      // 浏览器里没有系统目录选择框，用输入框代替。
      const picked = window.prompt(options?.title ?? "选择文件夹", options?.defaultPath ?? "");
      return picked && picked.trim() ? picked.trim() : null;
    },
    openUrl: async (url) => {
      window.open(url, "_blank", "noopener");
    },

    onFfmpegStatus: (handler) => ffmpegChannel.subscribe(handler),
    onTaskUpdated: (handler) => updatedChannel.subscribe(handler),
    onTaskRemoved: (handler) => removedChannel.subscribe(handler),
    onActivity: (handler) => activityChannel.subscribe(handler),
    getSchedulerStatus: () => Promise.resolve(schedulerStatus()),
    onSchedulerStatus: (handler) => schedulerChannel.subscribe(handler),
    getQuitState: () => Promise.resolve({ ...quit, recording: schedulerStatus().recording }),
    async confirmQuit() {
      setQuit("finishing");
      await delay(3000);
      console.info("[mock] 收尾完成，应用退出（浏览器里回到 idle）");
      setQuit("idle");
    },
    async cancelQuit() {
      if (quit.phase !== "finishing") setQuit("idle");
      return quit;
    },
    onQuitState: (handler) => quitChannel.subscribe(handler),
    async resolveClose(choice, remember) {
      if (remember) settings = { ...settings, close_window_action: choice };
      performClose(choice);
    },
    async confirmPauseAll() {
      pauseAll();
    },
    onClosePrompt: (handler) => closeChannel.subscribe(handler),
    onPauseAllPrompt: (handler) => pauseAllChannel.subscribe(handler),
  };

  (window as unknown as { __lubobaoMock: unknown }).__lubobaoMock = {
    setFfmpeg: (name: string) => {
      clearInterval(simulation);
      const status = ffmpegPresets[name];
      if (status) setFfmpeg(status);
    },
    simulateDownload,
    requestQuit,
    requestClose,
    setUpdate: (mode: MockUpdateMode) => {
      updateMode = mode;
      updateDownloaded = false;
    },
    requestPauseAll,
    goLive,
    endLive,
    setStopSupported: (value: boolean) => {
      stopSupported = value;
    },
    setEvents: (value: boolean) => {
      eventsEnabled = value;
    },
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
