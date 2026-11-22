# Repository Guidelines

## Project Structure

LuBoBao (录播宝) is a Tauri 2 desktop app for live-stream recording. The current scaffold demonstrates the TypeScript-to-Rust connection with a greeting form. Stream monitoring and recording are planned features.

- Edit the form markup in `index.html`, DOM event handlers in `src/main.ts`, and styles in `src/styles.css`. Frontend images live in `src/assets/`.
- Implement native commands in `src-tauri/src/lib.rs` and register them with `tauri::generate_handler!`. The desktop entry point is `src-tauri/src/main.rs`.
- Configure the window and installer in `src-tauri/tauri.conf.json`. Application icons live in `src-tauri/icons/`.

## Development and Build Commands

Run commands from the repository root. Install Node.js, npm, and stable Rust. On Windows, install WebView2 Runtime and Microsoft C++ Build Tools with the desktop C++ workload and Windows SDK.

- `npm ci` installs the dependencies pinned in `package-lock.json`.
- `npm run dev` starts Vite for browser-based layout work.
- `npm run tauri dev` starts Vite and opens the desktop app with native commands available.
- `npm run build` checks TypeScript and builds the frontend into `dist/`.
- `npm run tauri build` builds the desktop app and writes installers to `src-tauri/target/release/bundle/`.

## Coding Style

Use two spaces in TypeScript, HTML, and CSS. Follow the existing camelCase TypeScript names and kebab-case HTML IDs, such as `greetInputEl` and `greet-input`. Strict TypeScript checks include unused locals and parameters. Use four spaces and snake_case function names in Rust.

## Testing

The project has no automated test cases, frontend test runner, or coverage threshold yet. Place new Rust unit tests beside the implementation in a `#[cfg(test)]` module, name each test after the behavior it checks, and run `cargo test --manifest-path src-tauri/Cargo.toml`. To check the desktop connection manually, launch the app, enter a name, click **Greet**, and verify the response includes that name.

## Commits and Pull Requests

Git history is unavailable in this workspace. Use a short imperative commit subject such as `Fix greeting form submission`. In a pull request, describe the behavior change, link any relevant issue, and list the checks performed. Include a screenshot for visible changes.
