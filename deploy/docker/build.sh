#!/usr/bin/env bash
# 任意装了 docker 的 aarch64 机器上一键构建 boom-gateway 镜像。
#
# 用法（在仓库任意位置执行均可）：
#   ./deploy/docker/build.sh
#   IMAGE_TAG=boom-gateway:v0.1.0 ./deploy/docker/build.sh
#   BASE_IMAGE=quay.io/ascend/vllm-ascend:v0.27.1rc1-a3-openeuler ./deploy/docker/build.sh
#   RUST_VERSION=1.95.0 CRATES_MIRROR=https://rsproxy.cn/index/ ./deploy/docker/build.sh
#   BUILDER=legacy ./deploy/docker/build.sh    # 强制兼容路径（默认自动探测，一般无需）
#
# 构建成功后默认清理中间产物，docker images 里只留最终镜像：
#   - 旧的 boom-gateway:* tag（含 verify 冒烟镜像）
#   - 基镜像（boom-base 本地 tag + 同 tag 的 vllm-ascend 拉取源引用）
#   - buildkit 构建缓存
# 迭代构建想保住加速材料时：
#   KEEP_BASE=1    # 保留基镜像（免重新拉取 ~8.5GB）
#   KEEP_CACHE=1   # 保留 buildkit 缓存（免 dnf/rustup/cargo 全部重来）
#
# 前置条件：docker + 网络可达 quay.io / sh.rustup.rs / crates.io。
# docker 带 buildx 插件（常规新装都有）走 buildkit；老版本 docker 无 buildx
# 插件时自动回退 docker build 兼容路径（见下方构建入口自适应）。
# 内网镜像源参数见 deploy/docker/README.md。
set -euo pipefail

cd "$(dirname "$0")/../.."

IMAGE_TAG="${IMAGE_TAG:-boom-gateway:$(git rev-parse --short HEAD)}"
BASE_IMAGE="${BASE_IMAGE:-quay.io/ascend/vllm-ascend:v0.27.1rc1-openeuler}"
RUST_VERSION="${RUST_VERSION:-1.95.0}"

echo ">>> building $IMAGE_TAG (platform linux/arm64, base $BASE_IMAGE, rust $RUST_VERSION)"

# ── 构建入口自适应：优先 buildx；老版本 docker（无 buildx 插件）回退 docker build ──
# 老版 docker CLI 不认识 buildx 子命令，报 "unknown flag: --platform"（buildx
# 被当未知命令后，后续 flag 落到 docker 根命令上解析）。回退路径差异：
#   - 不传 --platform（服务器本机就是 arm64，原生构建）
#   - 不传 --load（docker build 的产物本来就直接进 daemon）
#   - RUN --mount=type=cache 是 BuildKit 专属语法，用 sed 现场生成去掉 cache
#     mount 的兼容 Dockerfile（单一源文件，不维护两份；代价是无 cargo 增量缓存）
#   - DOCKER_BUILDKIT=0 强制经典构建器：老 daemon（18.09/19.03）的集成 buildkit
#     做 COPY 要从 docker.io 拉 docker/dockerfile-copy 辅助镜像，内网必超时；
#     经典构建器无此依赖，去掉 mount 后的 Dockerfile 它也完全能构建
#   - build 容器 seccomp：老 daemon 默认 seccomp 白名单缺 clone3，glibc 2.34+
#     的基镜像（openEuler 24.03）pthread_create 走 clone3 被 EPERM 拦截 →
#     curl/dnf 报 "getaddrinfo() thread failed to start"。daemon 支持时自动带
#     --security-opt seccomp=unconfined（先探测——不少 daemon 不收 build 级
#     flag）；不支持时根治靠升级 daemon 或改其默认 seccomp profile
if [ -z "${BUILDER:-}" ]; then
    if docker buildx version >/dev/null 2>&1; then BUILDER=buildx; else BUILDER=legacy; fi
fi
case "$BUILDER" in
    buildx|legacy) ;;
    *) echo "BUILDER must be buildx or legacy (got: $BUILDER)" >&2; exit 1 ;;
esac

# 基镜像 manifest 解析偶发超时（quay.io 网络抖动），重试 5 次
BUILD_ARGS=(--build-arg BASE_IMAGE="$BASE_IMAGE" --build-arg RUST_VERSION="$RUST_VERSION")
[ -n "${RUSTUP_INSTALLER:-}" ] && BUILD_ARGS+=(--build-arg RUSTUP_INSTALLER="$RUSTUP_INSTALLER")
[ -n "${RUSTUP_DIST_SERVER:-}" ] && BUILD_ARGS+=(--build-arg RUSTUP_DIST_SERVER="$RUSTUP_DIST_SERVER")
[ -n "${RUSTUP_UPDATE_ROOT:-}" ] && BUILD_ARGS+=(--build-arg RUSTUP_UPDATE_ROOT="$RUSTUP_UPDATE_ROOT")
[ -n "${CRATES_MIRROR:-}" ] && BUILD_ARGS+=(--build-arg CRATES_MIRROR="$CRATES_MIRROR")
# 追加 cargo feature（如 debug-tools：Dashboard 的 L2 kvc_aware 选项 + Debug 页）
[ -n "${BUILD_FEATURES:-}" ] && BUILD_ARGS+=(--build-arg BUILD_FEATURES="$BUILD_FEATURES")

# 公司 MITM 代理的自签 CA：PROXY_CA 指向 PEM 文件，或含多个 PEM 的目录
# （/etc/pki/ca-trust/source/anchors/ 就是目录形态，里面正是管理员手工装的
# 公司 CA；最省事 = 整包宿主机 /etc/pki/tls/certs/ca-bundle.crt——宿主机能
# 走代理，公司 CA 已在其中）。内容写进 deploy/docker/proxy-ca.pem，随构建
# 上下文 COPY 进构建容器装信任库——不走 build-arg：整包 bundle 几百 KB，
# 超过 execve 单参数 128KB 上限，docker 命令直接 "Argument list too long"。
# 证书只在构建期生效、不进成品镜像；构建结束（含失败）恢复为空占位文件。
PROXY_CA_DST="deploy/docker/proxy-ca.pem"
if [ -n "${PROXY_CA:-}" ]; then
    if [ -d "$PROXY_CA" ]; then
        cat "$PROXY_CA"/*.pem "$PROXY_CA"/*.crt > "$PROXY_CA_DST" 2>/dev/null || true
        [ -s "$PROXY_CA_DST" ] \
            || { echo "PROXY_CA=$PROXY_CA 目录下没有 .pem/.crt 文件" >&2; exit 1; }
    elif [ -f "$PROXY_CA" ]; then
        grep -q 'BEGIN CERTIFICATE' "$PROXY_CA" \
            || { echo "PROXY_CA=$PROXY_CA 不像 PEM 证书（没有 BEGIN CERTIFICATE 行）" >&2; exit 1; }
        cat "$PROXY_CA" > "$PROXY_CA_DST"
    else
        echo "PROXY_CA=$PROXY_CA 文件不存在" >&2
        exit 1
    fi
else
    : > "$PROXY_CA_DST"
fi

# 退出清理：恢复 CA 占位文件 + 删 legacy 兼容 Dockerfile。INT/TERM 转 exit
# 以触发 EXIT trap（Ctrl-C 杀掉构建后占位文件也要还原；万一 kill -9 没跑成
# 清理，git status 会显示 proxy-ca.pem 被改，git checkout -- 还原即可——
# 证书是公开材料，不涉密）
DOCKERFILE_TMP=""
cleanup() {
    [ -n "$DOCKERFILE_TMP" ] && rm -f "$DOCKERFILE_TMP" || true
    : > "$PROXY_CA_DST" 2>/dev/null || true
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# 代理透传：公司网络常要求代理出网，而构建容器不继承宿主机 shell 的代理变量
# （宿主机 curl 通、容器里全超时就是这症状）。HTTP(S)_PROXY / ALL_PROXY /
# NO_PROXY 是 docker 预定义 build-arg：RUN 步里 curl/rustup/cargo 自动认，
# 且不进成品镜像——运行期 gateway 连内网上游不走代理。
# 代理指向 127.0.0.1/localhost 时容器内不可达（那是容器自身），本机代理需
# 换成宿主机网桥地址（如 http://172.17.0.1:7890），此处提示一下。
for _p in HTTP_PROXY http_proxy HTTPS_PROXY https_proxy ALL_PROXY all_proxy NO_PROXY no_proxy; do
    _v=$(printenv "$_p" 2>/dev/null || true)
    [ -n "$_v" ] && BUILD_ARGS+=(--build-arg "$_p=$_v")
done
case "${HTTPS_PROXY:-}${https_proxy:-}" in
    *127.0.0.1*|*localhost*)
        echo ">>> warning: 代理地址含 127.0.0.1/localhost——构建容器里它指向容器自身；本机代理需换成宿主机网桥地址（如 http://172.17.0.1:7890）" >&2
        ;;
esac

DOCKERFILE="deploy/docker/Dockerfile"
if [ "$BUILDER" = legacy ]; then
    DOCKERFILE_TMP=$(mktemp /tmp/boom-dockerfile.XXXXXX)
    sed 's/--mount=type=cache,[^ ]* //g' "$DOCKERFILE" > "$DOCKERFILE_TMP"
    # 清理走统一 EXIT trap（见上），此处只赋值
    DOCKERFILE="$DOCKERFILE_TMP"
    echo ">>> builder: legacy docker build (no buildx plugin; cache mounts stripped)"
    # 不少 daemon 不收 build 级 --security-opt（"does not support setting
    # security options on build"）——先探测，不收就不带。此时若 RUN 报
    # getaddrinfo() thread failed to start，需更新 daemon 的 seccomp profile
    #（老 daemon 缺 clone3，glibc 2.34+ 基镜像起不了线程）
    LEGACY_SECOPT=()
    if printf 'FROM scratch\n' | DOCKER_BUILDKIT=0 docker build --security-opt seccomp=unconfined - >/dev/null 2>&1; then
        LEGACY_SECOPT=(--security-opt seccomp=unconfined)
        echo ">>> build containers: seccomp=unconfined (clone3 workaround for old daemons)"
    fi
fi

run_build() {
    if [ "$BUILDER" = buildx ]; then
        docker buildx build --platform linux/arm64 --load "${BUILD_ARGS[@]}" \
            -t "$IMAGE_TAG" -f "$DOCKERFILE" .
    elif [ "${#LEGACY_SECOPT[@]}" -gt 0 ]; then
        DOCKER_BUILDKIT=0 docker build "${LEGACY_SECOPT[@]}" "${BUILD_ARGS[@]}" \
            -t "$IMAGE_TAG" -f "$DOCKERFILE" .
    else
        DOCKER_BUILDKIT=0 docker build "${BUILD_ARGS[@]}" \
            -t "$IMAGE_TAG" -f "$DOCKERFILE" .
    fi
}

ok=0
for attempt in 1 2 3 4 5; do
    if run_build; then
        ok=1
        break
    fi
    echo ">>> build attempt $attempt failed, retrying in 15s..."
    sleep 15
done
if [ "$ok" != 1 ]; then
    echo "build failed after 5 attempts (builder: $BUILDER)" >&2
    if [ "$BUILDER" = buildx ]; then
        echo "hint: 日志若含 'Failed to connect'/'Timeout was reached'（exit 28）→ 构建机出不了外网，走 README 场景 B 的 rsproxy 镜像源变量" >&2
        echo "hint: BUILDER=legacy 只适用于无 buildx 插件的老 docker；buildx 正常的机器别切（legacy 无 cache mount，每次全量编译）" >&2
    fi
    if [ "$BUILDER" = legacy ]; then
        echo "hint: 日志若含 'getaddrinfo() thread failed to start' → 老 daemon seccomp 缺 clone3，需升级 daemon 或改其默认 seccomp profile" >&2
    fi
    exit 1
fi

echo ">>> built $IMAGE_TAG"

# ── 构建成功后清理：docker images 只保留最终镜像 ─────────────────
# 注意：删基镜像的 tag 不释放磁盘（层与最终镜像共享），清理的意义是
# 列表干净；真正释放磁盘的是 builder prune（构建缓存，GB 级）。
cleanup_intermediates() {
    local removed=() t

    # 1. 其他 boom-gateway tag（旧版本 + verify 冒烟镜像）
    while IFS= read -r t; do
        [ -z "$t" ] && continue
        [ "$t" = "$IMAGE_TAG" ] && continue
        docker rmi "$t" >/dev/null 2>&1 && removed+=("$t")
    done < <(docker images --format '{{.Repository}}:{{.Tag}}' | grep -Ex 'boom-gateway:.+' || true)

    # 2. 基镜像族：boom-base:<tag> 与同 tag 的 */vllm-ascend（拉取源引用）
    if [ -z "${KEEP_BASE:-}" ]; then
        local base_tag="${BASE_IMAGE##*:}"
        while IFS= read -r t; do
            [ -z "$t" ] && continue
            docker rmi "$t" >/dev/null 2>&1 && removed+=("$t")
        done < <(docker images --format '{{.Repository}}:{{.Tag}}' \
                    | grep -E "(^boom-base|(^|/)vllm-ascend):${base_tag}\$" || true)
    fi

    # 3. 悬空层
    docker image prune -f >/dev/null 2>&1 || true

    # 4. buildkit 构建缓存（GB 级，真正的磁盘大头）
    #    必须带 --all：普通 prune 只清 dangling，带不走 cargo 的 cache mount
    if [ -z "${KEEP_CACHE:-}" ]; then
        docker builder prune --all -f >/dev/null 2>&1 || true
    fi

    if [ "${#removed[@]}" -gt 0 ]; then
        echo ">>> removed intermediate image tags (kept only $IMAGE_TAG):"
        printf '    %s\n' "${removed[@]}"
    fi
}
cleanup_intermediates

echo ">>> done. only $IMAGE_TAG kept (use KEEP_BASE=1 / KEEP_CACHE=1 to keep build inputs for iteration)"
