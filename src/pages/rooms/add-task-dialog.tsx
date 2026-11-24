import { useEffect, useId, useRef, useState, type FormEvent } from "react";
import { Loader2, Plus } from "lucide-react";

import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  api,
  isMockBackend,
  type Quality,
  type ResolvedRoom,
  type TaskId,
  type TaskSnapshot,
} from "@/lib/api";
import { AnchorAvatar, PlatformTag } from "@/pages/rooms/anchor";
import { describeError, qualityLabels, type ErrorView } from "@/pages/rooms/format";
import { Notice } from "@/pages/rooms/notice";
import { statusText } from "@/pages/rooms/task-status";
import { RecordOptions } from "@/pages/rooms/task-form-fields";

type Phase =
  | { step: "input" }
  | { step: "resolving"; force: boolean }
  | { step: "added"; task: TaskSnapshot }
  | { step: "duplicate"; existing: TaskSnapshot[]; candidate: ResolvedRoom }
  | { step: "error"; error: ErrorView; force: boolean };

/** 浏览器假数据下的示例链接，便于演示各种结果。 */
const mockExamples: { label: string; url: string }[] = [
  { label: "新直播间", url: "https://live.douyin.com/745964462470" },
  { label: "重复（主页短链）", url: "https://v.douyin.com/CeiU5cbX" },
  { label: "快手", url: "https://live.kuaishou.com/u/3xdemo" },
  { label: "B站", url: "https://live.bilibili.com/22603245" },
  { label: "虎牙", url: "https://www.huya.com/52333" },
  { label: "斗鱼", url: "https://www.douyu.com/topic/demo?rid=5201314" },
  { label: "小红书", url: "https://www.xiaohongshu.com/livestream/570555100739016375" },
  { label: "YY", url: "https://www.yy.com/22490906/22490906" },
  { label: "网易CC", url: "https://cc.163.com/583946984" },
  { label: "微博", url: "https://weibo.com/l/wblive/p/show/1022:2321325026370190442592" },
  { label: "需要登录", url: "https://live.douyin.com/login-required" },
  { label: "不支持的平台", url: "https://www.example.com/live/1" },
  { label: "网络错误", url: "https://live.douyin.com/network-timeout" },
];

interface AddTaskDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** 添加成功或用户选择查看已有任务后定位到该任务。 */
  onShowTask: (id: TaskId) => void;
}

export function AddTaskDialog({ open, onOpenChange, onShowTask }: AddTaskDialogProps) {
  const id = useId();
  const inputRef = useRef<HTMLInputElement>(null);
  // 父组件每次打开都换 key 重新挂载，草稿从空白开始，不会闪出上一次的内容。
  const [url, setUrl] = useState("");
  const [quality, setQuality] = useState<Quality>("OD");
  const [notifyOnly, setNotifyOnly] = useState(false);
  const [defaultQuality, setDefaultQuality] = useState<Quality | undefined>();
  const [phase, setPhase] = useState<Phase>({ step: "input" });
  const touched = useRef({ quality: false, notifyOnly: false });

  // 默认值取自设置；设置晚到时，不覆盖用户已经改过的字段。
  useEffect(() => {
    if (!open) return;
    let active = true;
    void api.getSettings().then(
      (settings) => {
        if (!active) return;
        setDefaultQuality(settings.record_quality);
        if (!touched.current.quality) setQuality(settings.record_quality);
        if (!touched.current.notifyOnly) setNotifyOnly(settings.only_notify_no_record);
      },
      () => undefined,
    );
    return () => {
      active = false;
    };
  }, [open]);

  const busy = phase.step === "resolving";

  async function submit(force: boolean) {
    const trimmed = url.trim();
    if (!trimmed || busy) return;
    setPhase({ step: "resolving", force });
    try {
      const result = await api.addTask({ url: trimmed, quality, notifyOnly, force });
      if (result.result === "duplicate") {
        setPhase({ step: "duplicate", existing: result.existing, candidate: result.candidate });
      } else {
        setPhase({ step: "added", task: result.task });
      }
    } catch (error) {
      setPhase({ step: "error", error: describeError(error), force });
    }
  }

  function onSubmit(event: FormEvent) {
    event.preventDefault();
    if (phase.step === "added") {
      // 再添加一个：清空输入。
      setUrl("");
      setPhase({ step: "input" });
      inputRef.current?.focus();
      return;
    }
    // 强制添加失败后点“重试”，仍按强制添加提交。
    void submit(phase.step === "error" && phase.force);
  }

  function editUrl(next: string) {
    setUrl(next);
    if (phase.step !== "resolving" && phase.step !== "input") setPhase({ step: "input" });
  }

  return (
    <Dialog open={open} onOpenChange={(next) => !busy && onOpenChange(next)}>
      <DialogContent
        className="sm:max-w-(--dialog-width)"
        onOpenAutoFocus={(event) => {
          event.preventDefault();
          inputRef.current?.focus();
        }}
      >
        <form onSubmit={onSubmit} className="flex flex-col gap-4">
          <DialogHeader>
            <DialogTitle>添加直播间</DialogTitle>
            <DialogDescription>粘贴直播间链接、主播主页或分享短链，录播宝会先识别主播再添加。</DialogDescription>
          </DialogHeader>

          <div className="flex flex-col gap-1.5">
            <Label htmlFor={`${id}-url`}>链接</Label>
            <Input
              id={`${id}-url`}
              ref={inputRef}
              value={url}
              onChange={(event) => editUrl(event.target.value)}
              placeholder="https://live.douyin.com/…"
              autoComplete="off"
              spellCheck={false}
              // 识别期间只读而非禁用，焦点留在输入框，出错后可直接修改。
              readOnly={busy}
              aria-busy={busy}
            />
            {isMockBackend && phase.step === "input" && (
              <div className="flex flex-wrap items-center gap-1 text-xs text-muted-foreground">
                假数据示例：
                {mockExamples.map((example) => (
                  <Button key={example.url} type="button" variant="link" size="xs" onClick={() => editUrl(example.url)}>
                    {example.label}
                  </Button>
                ))}
              </div>
            )}
          </div>

          <RecordOptions
            quality={quality}
            onQualityChange={(value) => {
              touched.current.quality = true;
              setQuality(value);
            }}
            notifyOnly={notifyOnly}
            onNotifyOnlyChange={(value) => {
              touched.current.notifyOnly = true;
              setNotifyOnly(value);
            }}
            defaultQuality={defaultQuality}
            disabled={busy || phase.step === "duplicate"}
          />

          <PhaseView
            phase={phase}
            onShowTask={(taskId) => {
              onOpenChange(false);
              onShowTask(taskId);
            }}
          />

          <DialogFooter>
            {phase.step === "duplicate" ? (
              <>
                <Button type="button" variant="outline" onClick={() => onOpenChange(false)}>
                  取消
                </Button>
                <Button type="button" autoFocus onClick={() => void submit(true)}>
                  仍然添加
                </Button>
              </>
            ) : phase.step === "added" ? (
              <>
                <Button type="submit" variant="outline">
                  <Plus aria-hidden />
                  继续添加
                </Button>
                <Button
                  type="button"
                  autoFocus
                  onClick={() => {
                    onOpenChange(false);
                    onShowTask(phase.task.id);
                  }}
                >
                  完成
                </Button>
              </>
            ) : (
              <>
                <Button type="button" variant="outline" onClick={() => onOpenChange(false)} disabled={busy}>
                  取消
                </Button>
                <Button type="submit" disabled={busy || !url.trim()}>
                  {busy && <Loader2 className="animate-spin" aria-hidden />}
                  {busy ? "正在识别…" : phase.step === "error" ? "重试" : "识别并添加"}
                </Button>
              </>
            )}
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

function PhaseView({ phase, onShowTask }: { phase: Phase; onShowTask: (id: TaskId) => void }) {
  switch (phase.step) {
    case "input":
      return null;
    case "resolving":
      return (
        <Notice tone="info" title={phase.force ? "正在添加…" : "正在识别…"}>
          正在向平台查询直播间和主播信息。
        </Notice>
      );
    case "added": {
      const { task } = phase;
      return (
        <Notice tone="success" title="已添加，开始监控">
          <span className="mt-1.5 flex items-center gap-2 text-foreground">
            <AnchorAvatar task={task} />
            <span className="min-w-0">
              <span className="block truncate font-medium">{task.anchorName}</span>
              <span className="flex items-center gap-1.5 text-xs text-muted-foreground">
                <PlatformTag task={task} />
                {task.roomId} · {qualityLabels[task.quality]}
                {task.notifyOnly && " · 仅通知"}
                {task.live.state === "live" ? " · 正在直播" : task.live.state === "offline" ? " · 当前未开播" : ""}
              </span>
            </span>
          </span>
        </Notice>
      );
    }
    case "duplicate":
      return (
        <Notice tone="warning" title={`${phase.candidate.anchorName}（${phase.candidate.platformName}）已在任务列表中`}>
          <span className="mb-1 block">可以查看已有任务，或者仍然添加一个新任务。</span>
          <ul className="mt-1 flex flex-col gap-1">
            {phase.existing.map((task) => (
              <li key={task.id} className="flex items-center gap-2 text-foreground">
                <AnchorAvatar task={task} className="size-[22px] text-[11px]" />
                <span className="min-w-0 flex-1 truncate">
                  {task.anchorName}
                  <span className="text-xs text-muted-foreground">
                    {" "}
                    · {task.platformName} · {task.roomId} · {statusText(task)}
                  </span>
                </span>
                <Button type="button" variant="outline" size="xs" onClick={() => onShowTask(task.id)}>
                  查看
                </Button>
              </li>
            ))}
          </ul>
        </Notice>
      );
    case "error":
      return (
        <Notice tone="error" title={phase.error.title}>
          {phase.error.advice && <span className="block">{phase.error.advice}</span>}
          {phase.error.message && <span className="mt-0.5 block text-xs">原因：{phase.error.message}</span>}
        </Notice>
      );
  }
}
