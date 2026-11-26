import { Activity, Clapperboard, Radio, Settings, type LucideIcon } from "lucide-react";

export type PageId = "rooms" | "library" | "activity" | "settings";

export interface PageInfo {
  id: PageId;
  label: string;
  icon: LucideIcon;
}

export const pages: readonly PageInfo[] = [
  {
    id: "rooms",
    label: "直播间",
    icon: Radio,
  },
  {
    id: "library",
    label: "录像库",
    icon: Clapperboard,
  },
  {
    id: "activity",
    label: "动态",
    icon: Activity,
  },
  {
    id: "settings",
    label: "设置",
    icon: Settings,
  },
];
