//! 录播宝核心：桌面端（`src-tauri`）和命令行程序（`lubobao-cli`）共用。
//!
//! 本 crate 不依赖 Tauri 或任何图形界面库；Tauri 命令、事件推送和桌面外壳都在 `src-tauri`。

pub mod ffmpeg;
pub mod library;
pub mod platforms;
pub mod recorder;
pub mod scheduler;
pub mod tasks;
pub mod ytdlp;
