# 录播宝命令行镜像（ghcr.io/seansongx/lubobao），支持 linux/amd64 和 linux/arm64。
#
# 构建上下文由 CI 准备（见 .github/workflows/cli.yml 的 “Prepare Docker context”）：
#   bin/<amd64|arm64>/lubobao-cli   musl 静态链接的发布版
#   e2e/<amd64|arm64>/              只用于 e2e 目标：带测试适配器的 lubobao-cli 和端到端测试程序
#   manifest.json                   FFmpeg 组件清单，与应用内置的 crates/lubobao-core/src/ffmpeg/manifest.json 相同
#   fetch-ffmpeg.mjs
#
# 目标平台上没有 RUN 步骤，构建多架构镜像不需要 QEMU。基础镜像按 digest 固定，升级时一起更新：
#   node:22-slim                          （多架构索引）
#   gcr.io/distroless/cc-debian12:nonroot （多架构索引）

# FFmpeg 在构建机上下载并按清单校验 SHA-256。
FROM --platform=$BUILDPLATFORM node:22-slim@sha256:e9f565d32859f0cbfb52125eafe8be39390f24dac302dc3cdf566e8ba3ab21ee AS ffmpeg
ARG TARGETARCH
WORKDIR /work
COPY manifest.json fetch-ffmpeg.mjs ./
RUN node fetch-ffmpeg.mjs manifest.json "$TARGETARCH" /out/opt/ffmpeg \
 && mkdir -p /out/data /out/recordings

# distroless cc：glibc 2.36、libgcc、ca-certificates、tzdata 和 nonroot 用户（uid 65532），没有 shell。
# @seankit/ffmpeg 的 Linux 版（BtbN 构建）动态链接 glibc 2.28 及以上，所以不能用 distroless static；
# lubobao-cli 本身是 musl 静态链接，不依赖这些库。
FROM gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f AS runtime
ARG TARGETARCH
LABEL org.opencontainers.image.source="https://github.com/seansongx/lubobao" \
      org.opencontainers.image.title="录播宝命令行" \
      org.opencontainers.image.description="录播宝：开播自动录，不用再蹲直播（无界面命令行版）。lubobao-cli 为 MIT 许可；内含的 FFmpeg（/opt/ffmpeg）为 GPL-3.0-or-later，许可证和源码位置见 /opt/ffmpeg/LICENSE、README.md" \
      org.opencontainers.image.licenses="MIT AND GPL-3.0-or-later"
COPY --from=ffmpeg /out/opt/ffmpeg /opt/ffmpeg
COPY --from=ffmpeg --chown=65532:65532 /out/data /data
COPY --from=ffmpeg --chown=65532:65532 /out/recordings /recordings
COPY --chmod=755 bin/${TARGETARCH}/lubobao-cli /usr/local/bin/lubobao-cli
ENV LUBOBAO_DATA_DIR=/data \
    LUBOBAO_SAVE_DIR=/recordings \
    LUBOBAO_FFMPEG=/opt/ffmpeg/ffmpeg \
    TZ=Asia/Shanghai
VOLUME ["/data", "/recordings"]
USER nonroot:nonroot
# 收尾最长约 30 秒：docker stop 请用 -t 45，compose 里设 stop_grace_period: 45s。
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/lubobao-cli"]
CMD ["run"]

# CI 用：在镜像里跑端到端测试（不发布）。
FROM runtime AS e2e
ARG TARGETARCH
COPY --chmod=755 e2e/${TARGETARCH}/lubobao-cli /usr/local/bin/lubobao-cli-e2e
COPY --chmod=755 e2e/${TARGETARCH}/e2e /usr/local/bin/lubobao-e2e
ENV LUBOBAO_E2E_CLI=/usr/local/bin/lubobao-cli-e2e \
    LUBOBAO_TEST_FFMPEG=/opt/ffmpeg/ffmpeg
ENTRYPOINT ["/usr/local/bin/lubobao-e2e", "--nocapture"]
CMD []
