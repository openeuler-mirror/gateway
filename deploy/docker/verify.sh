#!/usr/bin/env bash
# boom-gateway 容器本地冒烟：构建 → 运行 → 断言 → 优雅退出验证 → 清理。
#
# 宿主机只需要 docker + curl，不安装任何东西、不跑本地 cargo。
#
# 冒烟设计（沿用 deploy/docker/config/config.example.yaml 的老思路）：
#   - 不挂载 config，验证 entrypoint 的环境变量生成路径
#   - 上游指向容器内 discard 端口（127.0.0.1:9）—— 路由链路可用，
#     真发请求立刻连接失败，零外网依赖
#
# 断言项：
#   1. /health/live  == 200   （进程活性）
#   2. /health/ready == 200   （无 DB 配置时也 200）
#   3. POST /v1/chat/completions 鉴权通过（!= 401，上游错误属预期）
#   4. docker stop 后日志含 "Shutdown complete"（PID 1 SIGTERM 优雅退出）
#
# 用法：
#   ./deploy/docker/verify.sh          # 构建冒烟镜像 + 断言 + 清容器和冒烟镜像
#   ./deploy/docker/verify.sh --clean  # 同上，并额外清理 buildkit 构建缓存
set -euo pipefail

cd "$(dirname "$0")/../.."

IMAGE_TAG="boom-gateway:verify"
NAME="boom-gateway-verify"
HOST_PORT="${HOST_PORT:-14000}"
GATEWAY_PORT=4000
CLEAN=0
[ "${1:-}" = "--clean" ] && CLEAN=1

fail() { echo "VERIFY FAIL: $*" >&2; exit 1; }

# 容器清理兜底（成功/失败路径都执行）；legacy 路径生成的兼容 Dockerfile 和
# PROXY_CA 的占位文件一并清理/还原
DOCKERFILE_TMP=""
cleanup() {
    docker rm -f "$NAME" >/dev/null 2>&1 || true
    [ -n "$DOCKERFILE_TMP" ] && rm -f "$DOCKERFILE_TMP" || true
    : > "deploy/docker/proxy-ca.pem" 2>/dev/null || true
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# ── 1. 构建冒烟镜像 ─────────────────────────────────────────────
echo ">>> [1/4] building $IMAGE_TAG (linux/arm64)"
t0=$SECONDS
# 镜像源与 build.sh 同名透传（内网/弱网环境可指定）
# 构建入口自适应同 build.sh：无 buildx 插件的老 docker 回退 docker build
if [ -z "${BUILDER:-}" ]; then
    if docker buildx version >/dev/null 2>&1; then BUILDER=buildx; else BUILDER=legacy; fi
fi
case "$BUILDER" in
    buildx|legacy) ;;
    *) fail "BUILDER must be buildx or legacy (got: $BUILDER)" ;;
esac

BUILD_ARGS=(--build-arg BASE_IMAGE="${BASE_IMAGE:-quay.io/ascend/vllm-ascend:v0.27.1rc1-openeuler}")
[ -n "${RUSTUP_INSTALLER:-}" ] && BUILD_ARGS+=(--build-arg RUSTUP_INSTALLER="$RUSTUP_INSTALLER")
[ -n "${RUSTUP_DIST_SERVER:-}" ] && BUILD_ARGS+=(--build-arg RUSTUP_DIST_SERVER="$RUSTUP_DIST_SERVER")
[ -n "${RUSTUP_UPDATE_ROOT:-}" ] && BUILD_ARGS+=(--build-arg RUSTUP_UPDATE_ROOT="$RUSTUP_UPDATE_ROOT")
[ -n "${CRATES_MIRROR:-}" ] && BUILD_ARGS+=(--build-arg CRATES_MIRROR="$CRATES_MIRROR")
# PROXY_CA 同 build.sh：写进 deploy/docker/proxy-ca.pem 随上下文 COPY
# （不走 build-arg——整包 bundle 超 execve 单参数 128KB 上限），退出时还原
if [ -n "${PROXY_CA:-}" ]; then
    if [ -d "$PROXY_CA" ]; then
        cat "$PROXY_CA"/*.pem "$PROXY_CA"/*.crt > "deploy/docker/proxy-ca.pem" 2>/dev/null || true
        [ -s "deploy/docker/proxy-ca.pem" ] || fail "PROXY_CA=$PROXY_CA 目录下没有 .pem/.crt 文件"
    elif [ -f "$PROXY_CA" ]; then
        grep -q 'BEGIN CERTIFICATE' "$PROXY_CA" || fail "PROXY_CA=$PROXY_CA 不像 PEM 证书"
        cat "$PROXY_CA" > "deploy/docker/proxy-ca.pem"
    else
        fail "PROXY_CA=$PROXY_CA 文件不存在"
    fi
else
    : > "deploy/docker/proxy-ca.pem"
fi
for _p in HTTP_PROXY http_proxy HTTPS_PROXY https_proxy ALL_PROXY all_proxy NO_PROXY no_proxy; do
    _v=$(printenv "$_p" 2>/dev/null || true)
    [ -n "$_v" ] && BUILD_ARGS+=(--build-arg "$_p=$_v")
done

DOCKERFILE="deploy/docker/Dockerfile"
if [ "$BUILDER" = legacy ]; then
    DOCKERFILE_TMP=$(mktemp /tmp/boom-dockerfile.XXXXXX)
    sed 's/--mount=type=cache,[^ ]* //g' "$DOCKERFILE" > "$DOCKERFILE_TMP"
    DOCKERFILE="$DOCKERFILE_TMP"
    echo ">>> builder: legacy docker build (no buildx plugin; cache mounts stripped)"
    # 同 build.sh：探测 daemon 是否收 build 级 --security-opt，不收就不带
    LEGACY_SECOPT=()
    if printf 'FROM scratch\n' | DOCKER_BUILDKIT=0 docker build --security-opt seccomp=unconfined - >/dev/null 2>&1; then
        LEGACY_SECOPT=(--security-opt seccomp=unconfined)
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
        # DOCKER_BUILDKIT=0 经典构建器：老 daemon 集成 buildkit 的 COPY 依赖
        # docker.io 的 dockerfile-copy 辅助镜像，内网超时；经典构建器无此依赖
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
    if [ "$BUILDER" = legacy ]; then
        echo "hint: 日志若含 'getaddrinfo() thread failed to start' → 老 daemon seccomp 缺 clone3，需升级 daemon 或改其默认 seccomp profile" >&2
    fi
    fail "image build failed after 5 attempts (builder: $BUILDER)"
fi
echo ">>> build done in $((SECONDS - t0))s"

# ── 2. 运行：env 生成 config，上游指向 discard 端口 ─────────────
echo ">>> [2/4] starting container (host 127.0.0.1:$HOST_PORT -> container $GATEWAY_PORT)"
docker run -d --name "$NAME" \
    -p "127.0.0.1:${HOST_PORT}:${GATEWAY_PORT}" \
    -e MASTER_KEY=sk-verify-master \
    -e UPSTREAM_BASE_URL=http://127.0.0.1:9/v1 \
    -e UPSTREAM_MODEL=smoke-model \
    -e UPSTREAM_API_KEY=sk-dummy \
    "$IMAGE_TAG" >/dev/null || fail "docker run failed"

# ── 3. 断言 ─────────────────────────────────────────────────────
echo ">>> [3/4] waiting for /health/live (up to 60s)"
up=0
for _ in $(seq 1 60); do
    if curl -fsS --max-time 2 "http://127.0.0.1:${HOST_PORT}/health/live" >/dev/null 2>&1; then
        up=1
        break
    fi
    sleep 1
done
[ "$up" = 1 ] || { docker logs "$NAME" 2>&1 | tail -30; fail "/health/live never became healthy"; }
echo "    /health/live  : 200 OK"

live_code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:${HOST_PORT}/health/live")
[ "$live_code" = "200" ] || fail "/health/live returned $live_code, want 200"

ready_code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:${HOST_PORT}/health/ready")
[ "$ready_code" = "200" ] || fail "/health/ready returned $ready_code, want 200 (no DB configured)"
echo "    /health/ready : 200 OK"

# 鉴权 + 路由链路探针：master key 必须被接受（401 = 鉴权失败才算挂）。
# 上游是 discard 端口，5xx/503 属预期。
chat_code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 10 \
    -X POST "http://127.0.0.1:${HOST_PORT}/v1/chat/completions" \
    -H "Authorization: Bearer sk-verify-master" \
    -H "Content-Type: application/json" \
    -d '{"model":"smoke-model","messages":[{"role":"user","content":"hi"}],"max_tokens":1}')
[ "$chat_code" != "401" ] && [ "$chat_code" != "403" ] \
    || fail "chat completions returned $chat_code — auth rejected (routing/auth chain broken)"
echo "    chat probe    : HTTP $chat_code (non-401, auth+routing chain alive; upstream error expected)"

# ── 4. 优雅退出（PID 1 SIGTERM）────────────────────────────────
echo ">>> [4/4] docker stop (expect graceful shutdown in logs)"
docker stop -t 30 "$NAME" >/dev/null
docker logs "$NAME" 2>&1 | grep -q "Shutdown complete" \
    || { docker logs "$NAME" 2>&1 | tail -30; fail "graceful shutdown log line not found"; }
echo "    graceful shutdown confirmed (\"Shutdown complete\" in logs)"

echo
echo "VERIFY PASS ✅"

# 冒烟镜像是中间产物：通过即删（失败时保留，便于 docker logs / 复跑排查）
docker rmi "$IMAGE_TAG" >/dev/null 2>&1 || true
echo "(container and smoke image $IMAGE_TAG removed)"

if [ "$CLEAN" = 1 ]; then
    echo ">>> --clean: pruning buildkit cache"
    docker builder prune --all -f >/dev/null
    echo "done."
fi
