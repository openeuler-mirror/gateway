# boom-gateway 容器镜像（aarch64 / openEuler 底座）
#
# 形态：gateway 独立容器，纯 CPU，不需要 NPU 设备。
# 镜像不打包任何 config —— 运行期要么挂载（CONFIG_PATH 指定路径），
# 要么由 entrypoint.sh 用环境变量现场生成最小配置。
#
# 构建（任意装了 docker 的 aarch64 机器，或本脚本同目录的 build.sh）：
#   docker buildx build --platform linux/arm64 --load \
#     -t boom-gateway:dev -f deploy/docker/Dockerfile .
#
# 换底座（NPU 专属变体统一底座时）：--build-arg BASE_IMAGE=quay.io/ascend/vllm-ascend:v0.27.1rc1-a3-openeuler
# 内网镜像源：--build-arg RUSTUP_DIST_SERVER=... --build-arg CRATES_MIRROR=...（见 deploy/docker/README.md）
# 附加 cargo feature（如 debug-tools：Dashboard 的 L2 kvc_aware 选项 + Debug 页）：--build-arg BUILD_FEATURES=debug-tools

# ── 全局构建参数（在每个 stage 内使用前需重新 ARG 声明）──────────
# 基镜像：builder 与 runtime 用同一个 → glibc 完全一致，无兼容风险
ARG BASE_IMAGE=quay.io/ascend/vllm-ascend:v0.27.1rc1-openeuler
# 工具链锚点：与开发环境一致，跨构建机不漂移
ARG RUST_VERSION=1.95.0
# rustup 下载源（内网换镜像；rsproxy 有完整 dist，TUNA 缺 1.95.0 不可用）
ARG RUSTUP_INSTALLER=https://sh.rustup.rs
ARG RUSTUP_DIST_SERVER=https://static.rust-lang.org
ARG RUSTUP_UPDATE_ROOT=https://static.rust-lang.org/rustup
# crates.io 镜像（如 https://rsproxy.cn/index/；留空 = 直连）
ARG CRATES_MIRROR=
# 追加到默认 features（otlp）之后的 cargo feature，空格分隔。目前唯一用途：
# debug-tools——启用 Dashboard 的 L2 kvc_aware 选项与 Debug 页（window.__KVC_DEBUG
# 编译期注入，release 默认隐藏）。留空 = 现状。
ARG BUILD_FEATURES=

# ── stage 1: builder（同基镜像内编译，产物与运行时 userspace 一致）──
FROM ${BASE_IMAGE} AS builder

ARG RUST_VERSION
ARG RUSTUP_INSTALLER
ARG RUSTUP_DIST_SERVER
ARG RUSTUP_UPDATE_ROOT
ARG CRATES_MIRROR
ARG BUILD_FEATURES
# 系统信任库合并产物路径（openEuler/RHEL 族标准位置；update-ca-trust 生成）
ARG CA_BUNDLE=/etc/pki/tls/certs/ca-bundle.crt

# jemalloc（C）/ tokenizers（C++）需要编译工具链；rustup 需要 curl。
# vllm 镜像一般自带这套；dnf 失败（内网无 repo）时依赖预装，缺失则在此尽早失败。
RUN dnf install -y gcc gcc-c++ make curl ca-certificates || true \
 && command -v gcc \
 && command -v g++ \
 && command -v make \
 && command -v curl

# 公司 MITM 代理的自签 CA（build.sh 的 PROXY_CA 参数写入该文件；无参数时是
# 仓库里的空占位文件）。走构建上下文 COPY 而非 build-arg：整包宿主机 trust
# bundle 可达几百 KB，超过 execve 单参数/环境变量 128KB 上限，build-arg 传输
# 直接 "Argument list too long"，docker 命令都起不来。放 dnf 之后：CA 内容
# 变化不使 dnf 层缓存失效。
COPY deploy/docker/proxy-ca.pem /tmp/proxy-ca.pem

# 公司代理若是 TLS MITM（自签 CA 在证书链上），容器默认信任库不认 →
# curl/rustup/cargo 全部 exit 60。把 CA 装进构建容器信任库，并显式指向合并后
# 的 bundle（SSL_CERT_FILE：openssl 系/rustup 的 rustls-native-certs 都认；
# CURL_CA_BUNDLE：curl 工具）。只在 builder stage，运行期镜像不带代理痕迹。
RUN if [ -s /tmp/proxy-ca.pem ]; then \
      mkdir -p /etc/pki/ca-trust/source/anchors \
       && cp /tmp/proxy-ca.pem /etc/pki/ca-trust/source/anchors/proxy-ca.pem; \
      if command -v update-ca-trust >/dev/null 2>&1; then \
        update-ca-trust extract; \
      else \
        mkdir -p "$(dirname "$CA_BUNDLE")" \
          && cat /etc/pki/ca-trust/source/anchors/proxy-ca.pem >> "$CA_BUNDLE"; \
      fi; \
    fi
ENV SSL_CERT_FILE=${CA_BUNDLE} \
    CURL_CA_BUNDLE=${CA_BUNDLE}

# rustup 装进容器（不影响任何宿主机环境）。
# 先下载再执行，不用 `curl | sh` 管道：管道退出码取自 sh，curl 网络失败会被
# 吞成"成功"，产出无工具链的假层，拖到 cargo 步才炸出难懂的错误。
# --connect-timeout 30：出口被防火墙丢包时 curl 默认要干等 300s，5 次重试
# 就是 25 分钟纯等待；快速失败，把诊断交给重试和换镜像源。
# RUSTUP_INSTALLER 两种形态都收：脚本（sh.rustup.rs / rsproxy 的
# rustup-init.sh，#! 开头，sh 执行）和二进制直链（rsproxy dist/.../rustup-init，
# ELF 不能用 sh 跑）——下载后按文件头分派。
ENV RUSTUP_DIST_SERVER=${RUSTUP_DIST_SERVER} \
    RUSTUP_UPDATE_ROOT=${RUSTUP_UPDATE_ROOT}
RUN curl -sSf --connect-timeout 30 ${RUSTUP_INSTALLER} -o /tmp/rustup-init \
 && if [ "$(head -c 2 /tmp/rustup-init)" = '#!' ]; then \
      sh /tmp/rustup-init -y --profile minimal --default-toolchain ${RUST_VERSION}; \
    else \
      chmod +x /tmp/rustup-init \
      && /tmp/rustup-init -y --profile minimal --default-toolchain ${RUST_VERSION}; \
    fi
ENV PATH=/root/.cargo/bin:$PATH

# 可选 crates 镜像源（sparse 协议）；MITM 代理时给 cargo 显式指定 CA
# （cargo 的 libcurl 未必读环境变量，config 里写死最稳）
RUN if [ -n "$CRATES_MIRROR" ]; then \
      printf '[source.crates-io]\nreplace-with = "mirror"\n\n[source.mirror]\nregistry = "sparse+%s"\n' \
        "$CRATES_MIRROR" > /root/.cargo/config.toml; \
    fi \
 && if [ -s /tmp/proxy-ca.pem ]; then \
      printf '[http]\ncainfo = "%s"\n' "$CA_BUNDLE" >> /root/.cargo/config.toml; \
    fi

WORKDIR /src
COPY . .

# --locked 用仓库 Cargo.lock 保证可复现；.cargo/config.toml（tokio_unstable）
# 随源码带入，cargo 自动生效。cache mount 加速增量构建——注意 mount 里的
# 文件不进镜像层，产物必须在本 RUN 内拷出到普通路径。
# BUILD_FEATURES 非空时追加 --features（$FEAT 不加引号：空值展开为零个参数，
# 不产生空串 flag）；debug-tools 是零依赖 feature，不动 Cargo.lock，--locked 仍成立。
RUN --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/src/target \
    if [ -n "$BUILD_FEATURES" ]; then FEAT="--features $BUILD_FEATURES"; else FEAT=""; fi \
 && cargo build --release --locked -p boom-main $FEAT \
 && cp target/release/boom-gateway /boom-gateway

# ── stage 2: runtime ─────────────────────────────────────────────
FROM ${BASE_IMAGE}

COPY --from=builder /boom-gateway /app/bin/boom-gateway
COPY deploy/docker/entrypoint.sh /app/bin/entrypoint.sh
RUN chmod +x /app/bin/boom-gateway /app/bin/entrypoint.sh

# CONFIG_PATH 指向的文件不存在时，entrypoint 会用环境变量生成最小配置。
# PATH 前置 /app/bin：docker exec 进容器后可直接敲 boom-gateway / entrypoint.sh
ENV CONFIG_PATH=/app/config/config.yaml \
    RUST_LOG=info \
    PATH=/app/bin:$PATH

# 不声明 EXPOSE：容器监听端口由 GATEWAY_PORT / config 决定，对外映射交给
# 运行时 -p 指定（EXPOSE 只是元数据，本就不发布端口）

# /health/live 只探进程活性，不掺 DB/上游因素（与 scripts/healthcheck.sh 同口径）
HEALTHCHECK --interval=10s --timeout=3s --start-period=30s --retries=3 \
  CMD curl -fsS http://127.0.0.1:${GATEWAY_PORT:-4000}/health/live || exit 1

ENTRYPOINT ["/app/bin/entrypoint.sh"]
