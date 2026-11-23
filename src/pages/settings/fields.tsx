// 设置页的通用控件：分组卡片、字段行、开关、下拉、数字和文本输入。
// 开关和下拉改动后立即保存；输入框在失焦或按 Enter 时保存，Esc 撤销未保存的输入。

import { useEffect, useId, useRef, useState, type ReactNode } from "react";
import { RotateCcw } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectLabel,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { Textarea } from "@/components/ui/textarea";
import { cn } from "@/lib/utils";

interface SettingsCardProps {
  title: string;
  description?: ReactNode;
  /** 提供时在标题栏显示“恢复默认值”，需要点两次确认。 */
  onReset?: () => void;
  children: ReactNode;
}

export function SettingsCard({ title, description, onReset, children }: SettingsCardProps) {
  const headingId = useId();
  return (
    <section aria-labelledby={headingId} className="mb-6 max-w-(--settings-width) text-card-foreground">
      <header className="flex min-h-8 items-center gap-3 pb-2 pl-1">
        <div className="min-w-0 flex-1">
          <h2 id={headingId} className="m-0 text-(length:--text-ui) font-semibold mac:font-bold linux:font-bold">
            {title}
          </h2>
          {description ? <p className="m-0 mt-0.5 text-xs text-muted-foreground">{description}</p> : null}
        </div>
        {onReset ? <ResetButton onConfirm={onReset} /> : null}
      </header>
      {/* Windows：每行一张设置卡片；macOS：圆角分组表单；Linux：Adwaita 分组列表 */}
      <div className="flex flex-col gap-[3px] mac:gap-0 mac:divide-y mac:rounded-xl mac:border mac:border-black/5 mac:bg-card mac:dark:border-white/5 linux:gap-0 linux:divide-y linux:overflow-hidden linux:rounded-xl linux:border linux:bg-card linux:shadow-xs">
        {children}
      </div>
    </section>
  );
}

function ResetButton({ onConfirm }: { onConfirm: () => void }) {
  const [armed, setArmed] = useState(false);
  useEffect(() => {
    if (!armed) return;
    const timer = setTimeout(() => setArmed(false), 4000);
    return () => clearTimeout(timer);
  }, [armed]);
  return (
    <Button
      type="button"
      variant={armed ? "destructive" : "ghost"}
      size="sm"
      onClick={() => {
        if (armed) {
          setArmed(false);
          onConfirm();
        } else {
          setArmed(true);
        }
      }}
      onBlur={() => setArmed(false)}
      onKeyDown={(event) => {
        if (event.key === "Escape") setArmed(false);
      }}
    >
      <RotateCcw aria-hidden />
      {armed ? "再点一次确认" : "恢复默认值"}
    </Button>
  );
}

interface FieldRowProps {
  label: ReactNode;
  description?: ReactNode;
  /** 控件的 id，用于 label 关联。 */
  htmlFor?: string;
  error?: string;
  errorId?: string;
  disabled?: boolean;
  /** 控件较宽时放到标签下方。 */
  stacked?: boolean;
  children: ReactNode;
}

export function FieldRow({ label, description, htmlFor, error, errorId, disabled, stacked, children }: FieldRowProps) {
  return (
    <div
      className={cn(
        "px-4 py-3.5 win:flex win:min-h-[68px] win:flex-col win:justify-center win:rounded-lg win:border win:border-black/5 win:bg-card win:dark:border-black/15 mac:px-3 mac:py-2.5 linux:px-3.5 linux:py-3",
        disabled && "opacity-60",
      )}
    >
      <div className={cn("flex gap-x-5 gap-y-2", stacked ? "flex-col" : "flex-wrap items-center")}>
        <div className="min-w-[180px] flex-1">
          <label htmlFor={htmlFor} className="block text-(length:--text-ui)">
            {label}
          </label>
          {description ? <div className="mt-0.5 text-xs text-muted-foreground">{description}</div> : null}
        </div>
        <div className={cn("flex min-w-0 items-center gap-2", stacked ? "w-full" : "shrink-0")}>{children}</div>
      </div>
      {error ? (
        <p id={errorId} role="alert" className="m-0 mt-1.5 text-xs text-destructive">
          {error}
        </p>
      ) : null}
    </div>
  );
}

interface SwitchFieldProps {
  label: ReactNode;
  description?: ReactNode;
  checked: boolean;
  disabled?: boolean;
  error?: string;
  onChange: (checked: boolean) => void;
}

export function SwitchField({ label, description, checked, disabled, error, onChange }: SwitchFieldProps) {
  const id = useId();
  return (
    <FieldRow label={label} description={description} htmlFor={id} error={error} errorId={`${id}-error`} disabled={disabled}>
      <Switch
        id={id}
        checked={checked}
        disabled={disabled}
        aria-invalid={error ? true : undefined}
        aria-describedby={error ? `${id}-error` : undefined}
        onCheckedChange={onChange}
      />
    </FieldRow>
  );
}

export interface SelectOption {
  value: string;
  label: string;
  disabled?: boolean;
}

interface SelectFieldProps {
  label: ReactNode;
  description?: ReactNode;
  value: string;
  options: readonly SelectOption[] | readonly { group: string; options: readonly SelectOption[] }[];
  disabled?: boolean;
  error?: string;
  onChange: (value: string) => void;
}

/** 把连续的未分组选项合成一段，和带标题的分组一起按原顺序返回。 */
function groupOptions(options: SelectFieldProps["options"]) {
  const segments: ({ group: string; options: readonly SelectOption[] } | { items: SelectOption[] })[] = [];
  for (const entry of options) {
    if ("group" in entry) {
      segments.push(entry);
    } else {
      const last = segments[segments.length - 1];
      if (last && "items" in last) last.items.push(entry);
      else segments.push({ items: [entry] });
    }
  }
  return segments;
}

export function SelectField({ label, description, value, options, disabled, error, onChange }: SelectFieldProps) {
  const id = useId();
  const renderItem = (option: SelectOption) => (
    <SelectItem key={option.value} value={option.value} disabled={option.disabled}>
      {option.label}
    </SelectItem>
  );
  return (
    <FieldRow label={label} description={description} htmlFor={id} error={error} errorId={`${id}-error`} disabled={disabled}>
      <Select value={value} onValueChange={onChange} disabled={disabled}>
        <SelectTrigger
          id={id}
          className="w-60"
          aria-invalid={error ? true : undefined}
          aria-describedby={error ? `${id}-error` : undefined}
        >
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          {/* 按 shadcn 示例的结构：选项一律放在 SelectGroup 里，连续的未分组选项合成一组。 */}
          {groupOptions(options).map((entry, index) =>
            "group" in entry ? (
              <SelectGroup key={entry.group}>
                <SelectLabel>{entry.group}</SelectLabel>
                {entry.options.map(renderItem)}
              </SelectGroup>
            ) : (
              <SelectGroup key={`items-${index}`}>{entry.items.map(renderItem)}</SelectGroup>
            ),
          )}
        </SelectContent>
      </Select>
    </FieldRow>
  );
}

interface DraftInputOptions {
  value: string;
  /** 返回错误信息时不保存。 */
  validate?: (draft: string) => string | null;
  onCommit: (draft: string) => void;
  onError: (message: string | null) => void;
}

/**
 * 输入框草稿：外部值变化时同步，但正在编辑（草稿与上次保存的值不同）的不覆盖；
 * 失焦或 Enter 时校验并提交，Esc 撤销。
 */
function useDraft({ value, validate, onCommit, onError }: DraftInputOptions) {
  const [draft, setDraft] = useState(value);
  const committed = useRef(value);
  useEffect(() => {
    const previous = committed.current;
    committed.current = value;
    setDraft((current) => (current === previous ? value : current));
  }, [value]);

  const commit = () => {
    const message = validate?.(draft) ?? null;
    onError(message);
    if (message !== null) return;
    if (draft === committed.current) return;
    // 记下已提交的值：Enter 之后紧接着失焦不会把同一个值再保存一次。保存成功时外部值随之更新；
    // 保存失败时字段下有错误，再次修改输入后会重新提交。
    committed.current = draft;
    onCommit(draft);
  };

  return {
    value: draft,
    onChange: (event: React.ChangeEvent<HTMLInputElement | HTMLTextAreaElement>) => setDraft(event.target.value),
    onBlur: commit,
    onKeyDown: (event: React.KeyboardEvent<HTMLInputElement | HTMLTextAreaElement>) => {
      if (event.key === "Enter") {
        event.preventDefault();
        commit();
      } else if (event.key === "Escape") {
        setDraft(committed.current);
        onError(null);
      }
    },
  };
}

interface NumberFieldProps {
  label: ReactNode;
  description?: ReactNode;
  value: number;
  /** 只接受整数。 */
  integer?: boolean;
  /** 最小值（含），默认要求大于 0，与后端一致。 */
  min?: number;
  max?: number;
  suffix?: string;
  disabled?: boolean;
  error?: string;
  onCommit: (value: number) => void;
  onError: (message: string | null) => void;
}

export function NumberField({
  label,
  description,
  value,
  integer,
  min,
  max,
  suffix,
  disabled,
  error,
  onCommit,
  onError,
}: NumberFieldProps) {
  const id = useId();
  const draft = useDraft({
    value: String(value),
    validate: (text) => {
      const trimmed = text.trim();
      if (trimmed === "") return "请输入数值";
      const number = Number(trimmed);
      if (!Number.isFinite(number)) return "请输入数字";
      if (integer && !Number.isInteger(number)) return "请输入整数";
      if (min !== undefined ? number < min : number <= 0) {
        return min !== undefined ? `不能小于 ${min}` : "必须大于 0";
      }
      if (max !== undefined && number > max) return `不能大于 ${max}`;
      return null;
    },
    onCommit: (text) => onCommit(Number(text.trim())),
    onError,
  });
  return (
    <FieldRow label={label} description={description} htmlFor={id} error={error} errorId={`${id}-error`} disabled={disabled}>
      <Input
        id={id}
        inputMode={integer ? "numeric" : "decimal"}
        className="w-28 text-right tabular-nums"
        disabled={disabled}
        aria-invalid={error ? true : undefined}
        aria-describedby={error ? `${id}-error` : undefined}
        {...draft}
      />
      {/* 没有单位时也占位，让同一分组的输入框右侧对齐。 */}
      <span className="w-6 text-xs text-muted-foreground">{suffix}</span>
    </FieldRow>
  );
}

interface TextFieldProps {
  label: ReactNode;
  description?: ReactNode;
  value: string;
  placeholder?: string;
  disabled?: boolean;
  error?: string;
  stacked?: boolean;
  /** 多行显示，Enter 仍然是保存。 */
  multiline?: boolean;
  className?: string;
  validate?: (value: string) => string | null;
  /** 保存前整理输入，例如去掉首尾空格。 */
  normalize?: (value: string) => string;
  onCommit: (value: string) => void;
  onError: (message: string | null) => void;
  children?: ReactNode;
}

export function TextField({
  label,
  description,
  value,
  placeholder,
  disabled,
  error,
  stacked,
  multiline,
  className,
  validate,
  normalize = (text) => text.trim(),
  onCommit,
  onError,
  children,
}: TextFieldProps) {
  const id = useId();
  const draft = useDraft({
    value,
    validate: validate ? (text) => validate(normalize(text)) : undefined,
    onCommit: (text) => {
      const next = normalize(text);
      if (next !== value) onCommit(next);
    },
    onError,
  });
  return (
    <FieldRow
      label={label}
      description={description}
      htmlFor={id}
      error={error}
      errorId={`${id}-error`}
      disabled={disabled}
      stacked={stacked}
    >
      {multiline ? (
        <Textarea
          id={id}
          rows={2}
          placeholder={placeholder}
          className={cn("min-h-0 flex-1 resize-none", className)}
          disabled={disabled}
          spellCheck={false}
          aria-invalid={error ? true : undefined}
          aria-describedby={error ? `${id}-error` : undefined}
          {...draft}
        />
      ) : (
        <Input
          id={id}
          placeholder={placeholder}
          className={cn(stacked ? "flex-1" : "w-60", className)}
          disabled={disabled}
          spellCheck={false}
          autoComplete="off"
          aria-invalid={error ? true : undefined}
          aria-describedby={error ? `${id}-error` : undefined}
          {...draft}
        />
      )}
      {children}
    </FieldRow>
  );
}
