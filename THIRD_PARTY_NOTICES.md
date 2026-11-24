# Third-party notices

LuBoBao (录播宝) is licensed under the MIT License (see [LICENSE](LICENSE)). It includes or derives from the third-party works listed below.

## StreamGet

- Project: https://github.com/ihmily/streamget (v4.0.10, commit `e57e548`)
- License: MIT
- Used in: `crates/lubobao-core/src/platforms/sign.rs` is a Rust port of `streamget/platforms/douyin/ab_sign.py`. The Douyin adapter in `crates/lubobao-core/src/platforms/douyin.rs` follows StreamGet's request flow, headers, default cookies and quality handling. The Kuaishou adapter in `crates/lubobao-core/src/platforms/kuaishou.rs` is ported from `streamget/platforms/kuaishou/live_stream.py` (`KwaiLiveStream`): the cookie-only user-info check, page headers, reading `window.__INITIAL_STATE__` from the room page, and the bitrate-based quality mapping. The Bilibili adapter in `crates/lubobao-core/src/platforms/bilibili.rs` follows `streamget/platforms/bilibili/live_stream.py` (`BilibiliLiveStream`): the API request flow, headers, default cookie, the `qn` quality mapping and the `playUrl` host choice.
- Also used in: the Huya adapter in `crates/lubobao-core/src/platforms/huya.rs` is ported from `streamget/platforms/huya/live_stream.py` (`HuyaLiveStream`): the letter-id lookup, the `profileRoom` API flow and CDN choice, the web-page path for 英雄联盟 rooms, and the anti-leech parameter algorithm (`get_anti_code`).
- Also used in: the Douyu adapter in `crates/lubobao-core/src/platforms/douyu.rs` is ported from `streamget/platforms/douyu/live_stream.py` (`DouyuLiveStream`): the room id lookup, the `betard` room API, the `getEncryption` white-key MD5 signature for `getH5PlayV1` and the quality-to-rate mapping.
- Also used in: the Xiaohongshu adapter in `crates/lubobao-core/src/platforms/xiaohongshu.rs` is ported from `streamget/platforms/rednote/live_stream.py` (`RedNoteLiveStream`): the app request headers, the share-link redirect, reading `liveStream` from `window.__INITIAL_STATE__` on the room page, and building the source FLV/HLS URLs from the deeplink's `flvUrl`.
- Also used in: the YY adapter in `crates/lubobao-core/src/platforms/yy.rs` is ported from `streamget/platforms/yy/live_stream.py` (`YYLiveStream`): the page headers, reading the anchor and channel id from the room page, the `stream-manager` stream request body and the `live/detail` title lookup.
- Also used in: the NetEase CC adapter in `crates/lubobao-core/src/platforms/netease.rs` follows `streamget/platforms/netease/live_stream.py` (`NeteaseLiveStream`): the link form, request headers, result fields (`record_url = flv_url or m3u8_url`, `sharefile` as the HLS stream) and the position-based quality choice. The room data itself comes from CC's JSON APIs because the page StreamGet parses no longer exists.
- Also used in: the Weibo adapter in `crates/lubobao-core/src/platforms/weibo.rs` is ported from `streamget/platforms/weibo/live_stream.py` (`WeiboLiveStream`): the link forms, request headers and default visitor cookie, the `mymblog` lookup for profile links, the `pc/anchor/live` room API and deriving the source stream by removing the transcode suffix.
- Also used in: the Inke adapter in `crates/lubobao-core/src/platforms/inke.rs` follows `streamget/platforms/inke/live_stream.py` (`InkeLiveStream`): the link form (`uid` and `id` query parameters), the request headers and the `live_share_pc` anchor lookup. The stream itself comes from Inke's `now_publish` app API because `live_share_pc` no longer returns stream addresses.
- Also used in: the AcFun adapter in `crates/lubobao-core/src/platforms/acfun.rs` is ported from `streamget/platforms/acfun/live_stream.py` (`AcfunLiveStream`): the page headers and default cookie, the `userInfo` live check, the visitor login with a random `_did`, the `startPlay` request and sorting its FLV representations by bitrate.
- Also used in: the Huajiao adapter in `crates/lubobao-core/src/platforms/huajiao.rs` follows `streamget/platforms/huajiao/live_stream.py` (`HuajiaoLiveStream`): the request headers, the `getUserFeeds` live check (`rtop`), the FLV URL (`pull_url` plus `&codec={encode}`) and the `getFeedInfo` lookup of a broadcast id.
- Also used in: the Kugou adapter in `crates/lubobao-core/src/platforms/kugou.rs` is ported from `streamget/platforms/kugou/live_stream.py` (`KugouLiveStream`): the room id taken from the link, the page headers, the `getEnterRoomInfo` live check, the `mutiline/streamaddr` request parameters and taking the HTTPS FLV of the last stream line.
- Also used in: the 六间房 (6.cn) adapter in `crates/lubobao-core/src/platforms/sixroom.rs` is ported from `streamget/platforms/sixroom/live_stream.py` (`SixRoomLiveStream`): the iOS app request headers, reading the anchor uid from the room page, the `coop-mobile-inroom` form request and building the FLV address from `flvtitle`.
- Also used in: the Baidu adapter in `crates/lubobao-core/src/platforms/baidu.rs` is ported from `streamget/platforms/baidu/live_stream.py` (`BaiduLiveStream`): the `mbd.baidu.com/searchbox` `cmd=371` request, the `status == "0"` live check and rewriting the streams to the `hls.liveshow.bdstatic.com` HLS host.
- Also used in: the Zhihu adapter in `crates/lubobao-core/src/platforms/zhihu.rs` is ported from `streamget/platforms/zhihu/live_stream.py` (`ZhihuLiveStream`): the mobile request headers, reading `js-initialData` from the theater page, the `drama.status == 1` live check and the `playInfo` FLV and HLS URLs.
- Also used in: the Maoer FM adapter in `crates/lubobao-core/src/platforms/maoer.rs` is ported from `streamget/platforms/maoer/live_stream.py` (`MaoerLiveStream`): the `api/v2/live/{room_id}` request, the `status.broadcasting` live check and recording the FLV pull URL.
- Also used in: the TikTok adapter in `crates/lubobao-core/src/platforms/tiktok.rs` is ported from `streamget/platforms/tiktok/live_stream.py` (`TikTokLiveStream`): the PC and mobile request headers and default `ttwid` cookies, reading the room from the live page's `SIGI_STATE` script, the `api-live/user/room` request, the `status == 2` live check, the region-block message and building the quality list from `streamData.pull_data.stream_data` (sorted by video bitrate, then resolution, with the codec appended to each URL) with the neighbouring tier as fallback when the chosen stream cannot be reached.
- Also used in: the Twitch adapter in `crates/lubobao-core/src/platforms/twitch.rs` follows `streamget/platforms/twitch/live_stream.py` (`TwitchLiveStream`): the GQL request headers and web client id, the `streamPlaybackAccessToken` query, the `usher.ttvnw.net` master playlist request and its parameters, and choosing a variant by position after sorting by bandwidth. The channel and the token are read with one GQL query instead of StreamGet's persisted `ComscoreStreamingQuery`. `crates/lubobao-core/src/platforms/hls.rs` reads master playlists like StreamGet's `get_play_url_list`.
- Also used in: the YouTube adapter in `crates/lubobao-core/src/platforms/youtube.rs` follows `streamget/platforms/youtube/live_stream.py` (`YoutubeLiveStream`): reading `ytInitialPlayerResponse` from the page, the `videoDetails.isLive` check, recording `streamingData.hlsManifestUrl` and choosing a variant of that master playlist by position after sorting by bandwidth.

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

## yt-dlp and Deno (YouTube)

LuBoBao does not bundle these either. When a YouTube channel is first found live, it downloads the official single-file [yt-dlp](https://github.com/yt-dlp/yt-dlp) release (Unlicense; the release executables also contain yt-dlp-ejs and other dependencies under their own licenses, listed in the yt-dlp repository) and the official [Deno](https://github.com/denoland/deno) release (MIT License, Copyright 2018-2026 the Deno authors) from GitHub, checks their SHA-256 against `crates/lubobao-core/src/ytdlp/manifest.rs` and runs them as separate programs.

## FFmpeg

LuBoBao does not bundle FFmpeg. At runtime it downloads a verified FFmpeg build from the npm package [`@seankit/ffmpeg`](https://www.npmjs.com/package/@seankit/ffmpeg) and runs it as a separate program. Those builds are licensed under GPL-3.0-or-later; each package includes the license text and the location of the corresponding source code. The Docker image `ghcr.io/seansongx/lubobao` contains the same verified Linux build at `/opt/ffmpeg/ffmpeg`, with the package's `LICENSE` and `README.md` (upstream build and source location) next to it in `/opt/ffmpeg/`.
