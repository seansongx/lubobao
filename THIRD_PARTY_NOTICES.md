# Third-party notices

LuBoBao (录播宝) is licensed under the MIT License (see [LICENSE](LICENSE)). It includes or derives from the third-party works listed below.

## StreamGet

- Project: https://github.com/ihmily/streamget (v4.0.10, commit `e57e548`)
- License: MIT
- Used in: `src-tauri/src/platforms/sign.rs` is a Rust port of `streamget/platforms/douyin/ab_sign.py`. The Douyin adapter in `src-tauri/src/platforms/douyin.rs` follows StreamGet's request flow, headers, default cookies and quality handling.

```
MIT License

Copyright (c) 2025 Hmily

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## StreamCap

- Project: https://github.com/ihmily/StreamCap
- License: Apache License 2.0
- Used as: the behaviour reference for LuBoBao's features, defaults and FFmpeg options. LuBoBao reimplements this behaviour in Rust and TypeScript; deliberate differences are recorded in [docs/feature-map.md](docs/feature-map.md).

## FFmpeg

LuBoBao does not bundle FFmpeg. At runtime it downloads a verified FFmpeg build from the npm package [`@seankit/ffmpeg`](https://www.npmjs.com/package/@seankit/ffmpeg) and runs it as a separate program. Those builds are licensed under GPL-3.0-or-later; each package includes the license text and the location of the corresponding source code.
