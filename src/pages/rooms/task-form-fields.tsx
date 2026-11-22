import { useId } from "react";

import { Label } from "@/components/ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import type { Quality } from "@/lib/api";
import { qualityLabels, qualityOrder } from "@/pages/rooms/format";

interface RecordOptionsProps {
  quality: Quality;
  onQualityChange: (quality: Quality) => void;
  notifyOnly: boolean;
  onNotifyOnlyChange: (value: boolean) => void;
  defaultQuality?: Quality;
  disabled?: boolean;
}

/** 画质与“仅通知”，添加和编辑共用。 */
export function RecordOptions({
  quality,
  onQualityChange,
  notifyOnly,
  onNotifyOnlyChange,
  defaultQuality,
  disabled,
}: RecordOptionsProps) {
  const id = useId();
  return (
    <div className="grid grid-cols-2 gap-3">
      <div className="flex flex-col gap-1.5">
        <Label htmlFor={`${id}-quality`}>画质</Label>
        <Select value={quality} onValueChange={(value) => onQualityChange(value as Quality)} disabled={disabled}>
          <SelectTrigger id={`${id}-quality`} className="w-full">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {qualityOrder.map((item) => (
              <SelectItem key={item} value={item}>
                {qualityLabels[item]}
                {item === defaultQuality && "（默认）"}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </div>
      <div className="flex flex-col gap-1.5">
        <Label htmlFor={`${id}-notify`}>仅通知，不录制</Label>
        <div className="flex h-8 items-center gap-2">
          <Switch
            id={`${id}-notify`}
            checked={notifyOnly}
            onCheckedChange={onNotifyOnlyChange}
            disabled={disabled}
          />
          <span className="text-xs text-muted-foreground">{notifyOnly ? "开播时只提醒" : "开播后自动录制"}</span>
        </div>
      </div>
    </div>
  );
}
