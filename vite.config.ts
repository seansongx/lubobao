import path from "node:path";
import process from "node:process";
import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

const host = process.env.TAURI_DEV_HOST;

// https://vite.dev/config/
export default defineConfig(() => ({
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: {
      "@": path.resolve(import.meta.dirname, "./src"),
    },
  },

  // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
  //
  // 1. prevent Vite from obscuring rust errors
  clearScreen: false,
  // 2. tauri expects a fixed port, fail if that port is not available
  server: {
    port: 1420,
    strictPort: true,
    // 默认绑定 IPv4 回环：有的机器上 localhost 解析为 ::1 却连不通，Tauri 会一直等不到前端。
    host: host || "127.0.0.1",
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1421,
        }
      : undefined,
    watch: {
      // 3. tell Vite to ignore watching `src-tauri`, the Rust crates and the Cargo workspace's
      // `target/` at the repository root, and the session worktrees under this
      // project's own `.claude/`. Anchor it to the project root: worktrees live inside
      // `.claude/worktrees/`, so a bare `**/.claude/**` would also hide their own sources.
      ignored: [
        "**/src-tauri/**",
        "**/crates/**",
        "**/target/**",
        `${path.resolve(import.meta.dirname, ".claude").replaceAll("\\", "/")}/**`,
      ],
    },
  },
  // Only scan the app entry for dependencies, not HTML files in docs/, website/ or worktrees.
  optimizeDeps: {
    entries: ["index.html"],
  },
}));
