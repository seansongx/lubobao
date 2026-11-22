import { Activity, Clapperboard, Radio, Settings, type LucideIcon } from "lucide-react";

export type PageId = "rooms" | "library" | "activity" | "settings";

export interface PageInfo {
  id: PageId;
  label: string;
  icon: LucideIcon;
  emptyTitle: string;
  emptyDescription: string;
}

export const pages: readonly PageInfo[] = [
  {
    id: "rooms",
    label: "直播间",
    icon: Radio,
    emptyTitle: "还没有直播间",
    emptyDescription: "添加直播间后，录播宝会按检测间隔查看是否开播，开播后自动录制。任务列表将在后续版本提供。",
  },
  {
    id: "library",
    label: "录像库",
    icon: Clapperboard,
    emptyTitle: "还没有录像",
    emptyDescription: "录制完成的文件会按主播目录显示在这里，可以预览或在文件夹中定位。",
  },
  {
    id: "activity",
    label: "动态",
    icon: Activity,
    emptyTitle: "暂无动态",
    emptyDescription: "开播、开始录制、断流重试和需要处理的异常会按时间顺序显示在这里。",
  },
  {
    id: "settings",
    label: "设置",
    icon: Settings,
    emptyTitle: "设置暂未开放",
    emptyDescription: "录制默认值、输出命名、调度与并发、平台账号、通知渠道和组件管理将在后续版本提供。",
  },
];
