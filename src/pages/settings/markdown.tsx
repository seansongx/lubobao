// 只覆盖 THIRD_PARTY_NOTICES.md 和更新说明用到的 Markdown：标题、段落、列表、代码块、行内代码、粗体和链接。
// 链接交给系统浏览器打开；相对链接指向 GitHub 仓库中的同名文件。

import type { ReactNode } from "react";

import { api } from "@/lib/api";

const REPO_BLOB = "https://github.com/seansongx/lubobao/blob/main/";

type Block =
  | { type: "heading"; level: number; text: string }
  | { type: "paragraph"; text: string }
  | { type: "list"; items: string[] }
  | { type: "code"; text: string };

function parse(source: string): Block[] {
  const blocks: Block[] = [];
  const lines = source.replace(/\r\n/g, "\n").split("\n");
  let paragraph: string[] = [];
  let list: string[] | null = null;
  const flush = () => {
    if (paragraph.length) blocks.push({ type: "paragraph", text: paragraph.join(" ") });
    if (list) blocks.push({ type: "list", items: list });
    paragraph = [];
    list = null;
  };
  for (let index = 0; index < lines.length; index++) {
    const line = lines[index];
    if (line.startsWith("```")) {
      flush();
      const code: string[] = [];
      for (index++; index < lines.length && !lines[index].startsWith("```"); index++) code.push(lines[index]);
      blocks.push({ type: "code", text: code.join("\n") });
      continue;
    }
    const heading = /^(#{1,6})\s+(.*)$/.exec(line);
    if (heading) {
      flush();
      blocks.push({ type: "heading", level: heading[1].length, text: heading[2] });
      continue;
    }
    const item = /^\s*[-*]\s+(.*)$/.exec(line);
    if (item) {
      if (paragraph.length) flush();
      list ??= [];
      list.push(item[1]);
      continue;
    }
    if (line.trim() === "") {
      flush();
      continue;
    }
    if (list) list[list.length - 1] += ` ${line.trim()}`;
    else paragraph.push(line.trim());
  }
  flush();
  return blocks;
}

function resolveHref(href: string) {
  return /^https?:\/\//.test(href) ? href : REPO_BLOB + href.replace(/^\.?\//, "");
}

/**
 * 更新说明（来自服务器）里只有这些地址做成可点的链接，不能借它把用户引到别处：xiaofany.com、
 * lubobao.xiaofany.com 和本仓库。其他链接按纯文本显示。
 */
export function linkAllowed(url: string) {
  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch {
    return false;
  }
  if (parsed.protocol !== "https:" || parsed.username || parsed.password || parsed.port) return false;
  const host = parsed.hostname.toLowerCase();
  if (host === "xiaofany.com" || host === "lubobao.xiaofany.com") return true;
  return host === "github.com" && /^\/seansongx\/lubobao(\/|$)/i.test(parsed.pathname);
}

function Link({ href, restrict, children }: { href: string; restrict: boolean; children: ReactNode }) {
  const url = resolveHref(href);
  if (restrict && !linkAllowed(url)) {
    return <span>{children}</span>;
  }
  return (
    <a
      href={url}
      className="text-primary underline-offset-2 hover:underline"
      onClick={(event) => {
        event.preventDefault();
        void api.openUrl(url);
      }}
    >
      {children}
    </a>
  );
}

const INLINE = /`([^`]+)`|\[([^\]]+)\]\(([^)\s]+)\)|(https?:\/\/[^\s)]+[^\s).,;])|\*\*([^*]+)\*\*/g;

function inline(text: string, restrict: boolean): ReactNode[] {
  const out: ReactNode[] = [];
  let last = 0;
  for (const match of text.matchAll(INLINE)) {
    const at = match.index ?? 0;
    if (at > last) out.push(text.slice(last, at));
    const key = `${at}`;
    if (match[1] !== undefined) {
      out.push(
        <code key={key} className="rounded bg-muted px-1 font-mono text-[0.92em]">
          {match[1]}
        </code>,
      );
    } else if (match[2] !== undefined) {
      out.push(
        <Link key={key} href={match[3]} restrict={restrict}>
          {inline(match[2], restrict)}
        </Link>,
      );
    } else if (match[4] !== undefined) {
      out.push(
        <Link key={key} href={match[4]} restrict={restrict}>
          {match[4]}
        </Link>,
      );
    } else {
      out.push(
        <strong key={key} className="font-semibold text-foreground">
          {inline(match[5], restrict)}
        </strong>,
      );
    }
    last = at + match[0].length;
  }
  if (last < text.length) out.push(text.slice(last));
  return out;
}

/** restrictLinks：内容来自服务器（更新说明）时只放行 linkAllowed 的链接。 */
export function Markdown({ source, restrictLinks = false }: { source: string; restrictLinks?: boolean }) {
  return (
    <div className="space-y-2.5 text-[13px] leading-relaxed">
      {parse(source).map((block, index) => {
        switch (block.type) {
          case "heading":
            return block.level === 1 ? (
              <h3 key={index} className="m-0 text-sm font-semibold">
                {inline(block.text, restrictLinks)}
              </h3>
            ) : (
              <h4 key={index} className="m-0 pt-1.5 text-[13px] font-semibold">
                {inline(block.text, restrictLinks)}
              </h4>
            );
          case "paragraph":
            return (
              <p key={index} className="m-0 text-muted-foreground">
                {inline(block.text, restrictLinks)}
              </p>
            );
          case "list":
            return (
              <ul key={index} className="m-0 list-disc space-y-1 pl-5 text-muted-foreground">
                {block.items.map((item, itemIndex) => (
                  <li key={itemIndex}>{inline(item, restrictLinks)}</li>
                ))}
              </ul>
            );
          case "code":
            return (
              <pre
                key={index}
                className="m-0 overflow-x-auto rounded-md bg-muted p-3 font-mono text-[11px] leading-snug whitespace-pre-wrap"
              >
                {block.text}
              </pre>
            );
        }
      })}
    </div>
  );
}
