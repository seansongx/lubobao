import { useEffect, useRef, useState } from "react";
import { Copy } from "lucide-react";

import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuShortcut,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { isEditable } from "@/lib/native-behavior";
import { shortcutLabel } from "@/lib/platform";

/** 右键位置是否落在当前选区内（选区可能跨多行，逐个矩形判断）。 */
function pointInSelection(selection: Selection, x: number, y: number): boolean {
  for (let index = 0; index < selection.rangeCount; index += 1) {
    for (const rect of selection.getRangeAt(index).getClientRects()) {
      if (x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom) return true;
    }
  }
  return false;
}

interface MenuState {
  x: number;
  y: number;
  text: string;
}

/**
 * 可选中的文字（路径、链接、日志、错误原因）上有选区、且右键落在选区内时，弹出只含「复制」的应用菜单；
 * 其余位置的浏览器菜单由 native-behavior 屏蔽。
 */
export function SelectionMenu() {
  const [menu, setMenu] = useState<MenuState | null>(null);
  // 打开菜单前的焦点，关闭后还给它（默认会落到菜单的隐形锚点上）。
  const previousFocus = useRef<HTMLElement | null>(null);

  useEffect(() => {
    function onContextMenu(event: MouseEvent) {
      if (event.defaultPrevented || isEditable(event.target)) return;
      const target = event.target instanceof Element ? event.target : null;
      const selection = window.getSelection();
      const text = selection?.toString() ?? "";
      if (!selection || !text.trim() || !target?.closest(".select-text, [role=alert], [data-sonner-toast] [data-description]")) return;
      if (!pointInSelection(selection, event.clientX, event.clientY)) return;
      event.preventDefault();
      previousFocus.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
      setMenu({ x: event.clientX, y: event.clientY, text });
    }
    document.addEventListener("contextmenu", onContextMenu);
    return () => document.removeEventListener("contextmenu", onContextMenu);
  }, []);

  return (
    <DropdownMenu open={menu !== null} onOpenChange={(open) => !open && setMenu(null)} modal={false}>
      <DropdownMenuTrigger asChild>
        <span
          aria-hidden
          className="pointer-events-none fixed size-0"
          style={{ left: menu?.x ?? 0, top: menu?.y ?? 0 }}
        />
      </DropdownMenuTrigger>
      <DropdownMenuContent
        align="start"
        side="bottom"
        sideOffset={2}
        className="w-auto min-w-40"
        onCloseAutoFocus={(event) => {
          event.preventDefault();
          const target = previousFocus.current;
          previousFocus.current = null;
          if (target?.isConnected) target.focus({ preventScroll: true });
        }}
      >
        <DropdownMenuItem onSelect={() => menu && void navigator.clipboard.writeText(menu.text)}>
          <Copy aria-hidden />
          复制
          <DropdownMenuShortcut>{shortcutLabel("C")}</DropdownMenuShortcut>
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
