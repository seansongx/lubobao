import { useEffect, useId, useState, type FormEvent } from "react";
import { Loader2 } from "lucide-react";
import { toast } from "sonner";

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
import { Textarea } from "@/components/ui/textarea";
import { api, type Quality, type TaskSnapshot, type UpdateTaskInput } from "@/lib/api";
import { describeError, isRecordingActive, type ErrorView } from "@/pages/rooms/format";
import { Notice } from "@/pages/rooms/notice";
import { RecordOptions } from "@/pages/rooms/task-form-fields";

interface EditTaskDialogProps {
  task: TaskSnapshot | null;
  onOpenChange: (open: boolean) => void;
}

type Problem = { kind: "error"; error: ErrorView } | { kind: "duplicate"; existing: TaskSnapshot[] };

export function EditTaskDialog({ task, onOpenChange }: EditTaskDialogProps) {
  const id = useId();
  const [url, setUrl] = useState("");
  const [quality, setQuality] = useState<Quality>("OD");
  const [notifyOnly, setNotifyOnly] = useState(false);
  const [note, setNote] = useState("");
  const [saving, setSaving] = useState(false);
  const [problem, setProblem] = useState<Problem | null>(null);

  // 只在打开新任务时载入草稿，后端事件刷新快照不覆盖正在编辑的内容。
  const taskId = task?.id;
  useEffect(() => {
    if (!task) return;
    setUrl(task.url);
    setQuality(task.quality);
    setNotifyOnly(task.notifyOnly);
    setNote(task.note);
    setProblem(null);
    setSaving(false);
  }, [taskId]); // eslint-disable-line react-hooks/exhaustive-deps

  if (!task) return <Dialog open={false} />;
  const current = task;

  const changes: UpdateTaskInput = { id: current.id };
  if (url.trim() !== current.url) changes.url = url.trim();
  if (quality !== current.quality) changes.quality = quality;
  if (notifyOnly !== current.notifyOnly) changes.notifyOnly = notifyOnly;
  if (note !== current.note) changes.note = note;
  const dirty = Object.keys(changes).length > 1;
  const urlLocked = isRecordingActive(current.record);

  async function save(force: boolean) {
    if (!dirty || saving) return;
    if (changes.url !== undefined && !changes.url) {
      setProblem({ kind: "error", error: { title: "链接不能为空", advice: "", message: "" } });
      return;
    }
    setSaving(true);
    setProblem(null);
    try {
      const result = await api.updateTask({ ...changes, force: force || undefined });
      if (result.result === "duplicate") {
        setProblem({ kind: "duplicate", existing: result.existing });
      } else {
        toast.success(`已保存 ${result.task.anchorName}`);
        onOpenChange(false);
      }
    } catch (error) {
      setProblem({ kind: "error", error: describeError(error) });
    } finally {
      setSaving(false);
    }
  }

  function onSubmit(event: FormEvent) {
    event.preventDefault();
    void save(false);
  }

  return (
    <Dialog open onOpenChange={(next) => !saving && onOpenChange(next)}>
      <DialogContent className="sm:max-w-[520px]">
        <form onSubmit={onSubmit} className="flex flex-col gap-4">
          <DialogHeader>
            <DialogTitle>编辑 {current.anchorName}</DialogTitle>
            <DialogDescription>
              {current.platformName} · {current.roomId}
            </DialogDescription>
          </DialogHeader>

          <div className="flex flex-col gap-1.5">
            <Label htmlFor={`${id}-url`}>链接</Label>
            <Input
              id={`${id}-url`}
              value={url}
              onChange={(event) => {
                setUrl(event.target.value);
                setProblem(null);
              }}
              spellCheck={false}
              autoComplete="off"
              readOnly={saving}
            />
            {urlLocked && (
              <p className="m-0 text-xs text-muted-foreground">正在录制，修改链接会被拒绝；停止录制后再修改。</p>
            )}
          </div>

          <RecordOptions
            quality={quality}
            onQualityChange={setQuality}
            notifyOnly={notifyOnly}
            onNotifyOnlyChange={setNotifyOnly}
            disabled={saving}
          />

          <div className="flex flex-col gap-1.5">
            <Label htmlFor={`${id}-note`}>备注</Label>
            <Textarea
              id={`${id}-note`}
              value={note}
              onChange={(event) => setNote(event.target.value)}
              placeholder="只在录播宝内显示"
              className="min-h-16"
              readOnly={saving}
            />
          </div>

          {problem?.kind === "error" && (
            <Notice tone="error" title={problem.error.title}>
              {problem.error.message || problem.error.advice}
            </Notice>
          )}
          {problem?.kind === "duplicate" && (
            <Notice tone="warning" title="新链接与已有任务重复">
              {problem.existing.map((item) => `${item.anchorName}（${item.platformName} · ${item.roomId}）`).join("、")}
            </Notice>
          )}

          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => onOpenChange(false)} disabled={saving}>
              取消
            </Button>
            {problem?.kind === "duplicate" ? (
              <Button type="button" autoFocus onClick={() => void save(true)} disabled={saving}>
                仍然保存
              </Button>
            ) : (
              <Button type="submit" disabled={!dirty || saving}>
                {saving && <Loader2 className="animate-spin" aria-hidden />}
                保存
              </Button>
            )}
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
