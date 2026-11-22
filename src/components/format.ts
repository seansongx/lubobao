// 页面共用的格式化函数：字节、日期时间、时长。

const UNITS = ["B", "KB", "MB", "GB", "TB"];

/** 1024 进制，三位有效数字左右：`812 MB`、`5.62 GB`。 */
export function formatBytes(bytes: number | null | undefined): string {
  if (bytes === null || bytes === undefined || !Number.isFinite(bytes)) return "—";
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < UNITS.length - 1) {
    value /= 1024;
    unit += 1;
  }
  const digits = unit === 0 || value >= 100 ? 0 : value >= 10 ? 1 : 2;
  return `${value.toFixed(digits)} ${UNITS[unit]}`;
}

const pad = (value: number) => String(value).padStart(2, "0");

export function formatClock(ms: number, seconds = false): string {
  const date = new Date(ms);
  const base = `${pad(date.getHours())}:${pad(date.getMinutes())}`;
  return seconds ? `${base}:${pad(date.getSeconds())}` : base;
}

/** 今年省略年份：`11-22 20:14`；其他年份 `2025-11-22 20:14`。 */
export function formatDateTime(ms: number | null | undefined): string {
  if (ms === null || ms === undefined) return "—";
  const date = new Date(ms);
  const day = `${pad(date.getMonth() + 1)}-${pad(date.getDate())}`;
  const prefix = date.getFullYear() === new Date().getFullYear() ? day : `${date.getFullYear()}-${day}`;
  return `${prefix} ${formatClock(ms)}`;
}

function startOfDay(ms: number): number {
  const date = new Date(ms);
  date.setHours(0, 0, 0, 0);
  return date.getTime();
}

const WEEKDAYS = ["周日", "周一", "周二", "周三", "周四", "周五", "周六"];

/** 本地日期的分组键，用于按天分组。 */
export function dayKey(ms: number): number {
  return startOfDay(ms);
}

/** `今天`、`昨天`、`11月20日 周五`，跨年时带年份。 */
export function formatDay(dayStart: number): string {
  const today = startOfDay(Date.now());
  if (dayStart === today) return "今天";
  // 按日期比较，避开夏令时等非 24 小时的日子。
  const yesterday = new Date(today);
  yesterday.setDate(yesterday.getDate() - 1);
  if (dayStart === yesterday.getTime()) return "昨天";
  const date = new Date(dayStart);
  const year = date.getFullYear() === new Date().getFullYear() ? "" : `${date.getFullYear()}年`;
  return `${year}${date.getMonth() + 1}月${date.getDate()}日 ${WEEKDAYS[date.getDay()]}`;
}

/** `1:05:09`、`12:03`。 */
export function formatDuration(ms: number): string {
  const total = Math.max(0, Math.round(ms / 1000));
  const hours = Math.floor(total / 3600);
  const minutes = Math.floor((total % 3600) / 60);
  const seconds = total % 60;
  return hours > 0 ? `${hours}:${pad(minutes)}:${pad(seconds)}` : `${minutes}:${pad(seconds)}`;
}
