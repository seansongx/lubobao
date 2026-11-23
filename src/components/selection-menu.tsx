import { useEffect, useState } from "react";
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

interface MenuState {
  x: number;
  y: number;
  text: string;
}

/**
 * 可选中的文字（路径、链接、日志，带 select-text）上有选区时，右键弹出只含「复制」的应用菜单；
 * 其余位置的浏览器菜单由 native-behavior 屏蔽。
 */
export function SelectionMenu() {
  const [menu, setMenu] = useState<MenuState | null>(null);

  useEffect(() => {
    function onContextMenu(event: MouseEvent) {
      if (event.defaultPrevented || isEditable(event.target)) return;
      const target = event.target instanceof Element ? event.target : null;
      const text = window.getSelection()?.toString() ?? "";
      if (!text.trim() || !target?.closest(".select-text")) return;
      event.preventDefault();
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
      <DropdownMenuContent align="start" side="bottom" sideOffset={2} className="w-auto min-w-40">
        <DropdownMenuItem onSelect={() => menu && void navigator.clipboard.writeText(menu.text)}>
          <Copy aria-hidden />
          复制
          <DropdownMenuShortcut>{shortcutLabel("C")}</DropdownMenuShortcut>
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
