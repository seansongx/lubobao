import { clsx, type ClassValue } from "clsx";
import { extendTailwindMerge } from "tailwind-merge";

// index.css 的 @theme 里自定义了字号 text-dense、text-nav、text-title；
// 不告诉 tailwind-merge 的话，它会把这些当成文字颜色，与 text-muted-foreground 等合并时丢掉字号
const twMerge = extendTailwindMerge({
  extend: {
    theme: {
      text: ["dense", "nav", "title"],
    },
  },
});

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}
