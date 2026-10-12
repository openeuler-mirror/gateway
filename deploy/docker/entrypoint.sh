#!/bin/bash
# boom-gateway 容器入口。
#
# 职责：
#   1. CONFIG_PATH 存在 → 原样使用（挂载的正式配置，YAML 里原生支持
#      ${VAR} / os.environ/VAR 环境变量展开）。
#   2. CONFIG_PATH 不存在 → 用环境变量现场生成最小配置（镜像不打包 config）。
#      必填：MASTER_KEY、UPSTREAM_BASE_URL；其余有默认值。
#   3. exec boom-gateway 成为 PID 1 —— SIGTERM 走优雅退出
#      （广播停后台任务 → 关 DB pool → flush OTLP）。
#
# 注意：生成的配置写在容器可写层，docker rm 后重建容器会丢失；
# 正式部署请挂载 config 或固定用环境变量（见 deploy/docker/README.md）。
set -euo pipefail

CONFIG_PATH="${CONFIG_PATH:-/app/config/config.yaml}"

if [ ! -f "$CONFIG_PATH" ]; then
    : "${MASTER_KEY:?no config mounted at $CONFIG_PATH and MASTER_KEY is not set}"
    : "${UPSTREAM_BASE_URL:?no config mounted at $CONFIG_PATH and UPSTREAM_BASE_URL is not set}"
    mkdir -p "$(dirname "$CONFIG_PATH")"
    cat > "$CONFIG_PATH" <<EOF
# 由 entrypoint.sh 根据环境变量生成（容器可写层，重建即失）
model_list:
  - model_name: "*"
    litellm_params:
      model: openai/${UPSTREAM_MODEL:-default}
      api_key: ${UPSTREAM_API_KEY:-sk-dummy}
      api_base: ${UPSTREAM_BASE_URL}
general_settings:
  master_key: ${MASTER_KEY}
server:
  host: 0.0.0.0
  port: ${GATEWAY_PORT:-4000}
  workers: ${GATEWAY_WORKERS:-8}
EOF
    echo "entrypoint: generated minimal config at $CONFIG_PATH from env vars"
fi

ARGS=(-c "$CONFIG_PATH")
[ -n "${GATEWAY_HOST:-}" ] && ARGS+=(--host "$GATEWAY_HOST")
[ -n "${GATEWAY_PORT:-}" ] && ARGS+=(--port "$GATEWAY_PORT")

# "$@" 允许 docker run <image> --reboot 之类的参数透传
exec /app/bin/boom-gateway "${ARGS[@]}" "$@"
