#!/usr/bin/env bash
#
# boom-gateway 心跳检测 watchdog
#
# 周期性探测网关 /health/live,连续失败到阈值后调用用户提供的重启脚本,
# 然后冷却若干秒再恢复探测。设计为与网关同容器常驻运行。
#
# 探测端点选 /health/live(只验证 axum 路由还活,不掺 DB / config 因素)——
# DB 故障不应该触发网关重启,因为重启网关修不了 DB。
#
# 用法:
#   ./scripts/healthcheck.sh <port> <restart_script> <interval_secs>
#
# 参数(全部必填,位置参数):
#   $1  port             网关服务端口(如 8080)
#   $2  restart_script   重启脚本的绝对路径,失败时由本脚本调用
#   $3  interval_secs    心跳探测间隔(秒)
#
# 示例:
#   ./scripts/healthcheck.sh 8080 /app/restart.sh 5
#
# 行为约定:
#   - 探测超时(单次 curl):TIMEOUT=3s
#   - 连续失败阈值:MAX_FAILURES=3
#   - 重启后冷却:RESTART_WAIT=10s(老板指定)
#   - 启动宽容期:STARTUP_GRACE=30s(脚本启动后这段时间内失败不计入阈值,
#     防止网关冷启动慢被误判死亡)
#   - 重启频率上限:MAX_RESTARTS=5 / RESTART_WINDOW=1800s(30 分钟内重启
#     超过 5 次则放弃,防止"网关起不来 → 无限重启"烧 CPU)
#   - 单实例锁:LOCK_FILE,防止两个 watchdog 同时跑导致重复触发重启
#
# 退出码:
#   1 参数错误 / 锁获取失败 / curl 不存在
#   2 达到 MAX_RESTARTS 上限,主动放弃
#
# 日志全部走 stderr,stdout 不输出(让 docker logs 直接收集 stderr 即可)。

set -uo pipefail

# ── 可调常量(老板要改直接编辑这里)─────────────────────────────
readonly TIMEOUT=3              # 单次 curl 超时秒数
readonly MAX_FAILURES=3         # 连续失败几次触发重启
readonly RESTART_WAIT=10        # 重启后冷却秒数
readonly STARTUP_GRACE=30       # 启动宽容期秒数(期间失败不计入)
readonly MAX_RESTARTS=5         # 计数窗口内最大重启次数
readonly RESTART_WINDOW=1800    # 计数窗口秒数(默认 30 分钟)
readonly ENDPOINT="/health/live"
readonly LOCK_FILE="${HEALTHCHECK_LOCK_FILE:-/var/run/boom-healthcheck.lock}"
readonly LOG_PREFIX="healthcheck"

# ── 参数校验 ──────────────────────────────────────────────────
if [[ $# -ne 3 ]]; then
  echo "用法: $0 <port> <restart_script> <interval_secs>" >&2
  echo "示例: $0 8080 /app/restart.sh 5" >&2
  exit 1
fi

PORT="$1"
RESTART_SCRIPT="$2"
INTERVAL="$3"

if ! [[ "$PORT" =~ ^[1-9][0-9]{0,4}$ ]]; then
  echo "$LOG_PREFIX: 错误: port 必须是 1-65535 的正整数,收到: $PORT" >&2
  exit 1
fi
if (( PORT > 65535 )); then
  echo "$LOG_PREFIX: 错误: port 超出 65535,收到: $PORT" >&2
  exit 1
fi
if [[ ! -x "$RESTART_SCRIPT" ]]; then
  # 不要求 +x 时,[[ -x ]] 失败;降级检查文件存在并可读,
  # 让用户用 sh restart.sh 这种调用方式也能跑。
  if [[ ! -f "$RESTART_SCRIPT" ]]; then
    echo "$LOG_PREFIX: 错误: restart_script 不存在: $RESTART_SCRIPT" >&2
    exit 1
  fi
fi
if ! [[ "$INTERVAL" =~ ^[1-9][0-9]*$ ]]; then
  echo "$LOG_PREFIX: 错误: interval_secs 必须是正整数,收到: $INTERVAL" >&2
  exit 1
fi

command -v curl >/dev/null 2>&1 || {
  echo "$LOG_PREFIX: 错误: 缺少 curl" >&2
  exit 1
}

# ── 单实例锁(nice-to-have,环境不支持时 warn 继续)────────────
# 在标准 Linux 容器里(/var/run 可写 + util-linux 的 flock 存在)单实例锁
# 生效;在 macOS 或非 root 容器里(不可写 / 没 flock)打 warn 跳过,不阻塞
# 主功能。lock 是防止误启动两个实例,容器入口若用 supervisord/s6 管进程,
# 通常只会拉起一个 watchdog,这个保护更多是兜底。
# LOCK_FILE 在常量区已用 ${HEALTHCHECK_LOCK_FILE:-...} 形式声明,
# 用户可通过环境变量覆盖路径(例如改为 /tmp/boom-healthcheck.lock)。
if command -v flock >/dev/null 2>&1; then
  if exec 9>"$LOCK_FILE" 2>/dev/null; then
    if ! flock -n 9; then
      echo "$LOG_PREFIX: 另一个实例已在运行(锁 $LOCK_FILE 被持有),退出" >&2
      exit 1
    fi
  else
    echo "$LOG_PREFIX: 警告: 无法写 $LOCK_FILE (非 root?),跳过单实例保护" >&2
  fi
else
  echo "$LOG_PREFIX: 警告: 缺少 flock 命令,跳过单实例保护" >&2
fi

# ── 日志辅助 ──────────────────────────────────────────────────
# ISO8601 UTC 时间戳,方便 grep / 对账容器日志。
log() {
  local level="$1"; shift
  local ts
  ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "$LOG_PREFIX: $ts [$level] $*" >&2
}

log info "启动 (port=$PORT interval=${INTERVAL}s timeout=${TIMEOUT}s max_failures=$MAX_FAILURES restart_wait=${RESTART_WAIT}s grace=${STARTUP_GRACE}s max_restarts=$MAX_RESTARTS/$RESTART_WINDOW)"
log info "探测端点: http://127.0.0.1:$PORT$ENDPOINT"
log info "重启脚本: $RESTART_SCRIPT"

# ── 主循环 ────────────────────────────────────────────────────
fail_count=0
restarts_in_window=0
window_start=$SECONDS
script_start=$SECONDS
in_grace=1   # 启动后 STARTUP_GRACE 秒内不计数

while true; do
  # 重启计数窗口滑动:超过窗口则清零,给"配置错误后来修好了"留出空间。
  if (( SECONDS - window_start > RESTART_WINDOW )); then
    if (( restarts_in_window > 0 )); then
      log info "重启计数窗口过期,清零 (was $restarts_in_window)"
    fi
    restarts_in_window=0
    window_start=$SECONDS
  fi

  # 单次探测。-f 让 curl 在 4xx/5xx 时返回非零;-sS 关进度条但保留错误;
  # --max-time 防止 curl 自己挂死在卡死的连接上。
  if curl --max-time "$TIMEOUT" -fsS \
        "http://127.0.0.1:$PORT$ENDPOINT" >/dev/null 2>&1; then
    if (( fail_count > 0 )); then
      log info "探测恢复 (was $fail_count/$MAX_FAILURES)"
    fi
    fail_count=0
  else
    # 启动宽容期内的失败不计入阈值,但仍打日志,方便诊断"网关到底什么时候起来的"。
    if (( in_grace )); then
      log warn "探测失败,启动宽容期内不计入 (grace remaining: $((STARTUP_GRACE - (SECONDS - script_start)))s)"
    else
      fail_count=$((fail_count + 1))
      log warn "探测失败 ($fail_count/$MAX_FAILURES)"
    fi
  fi

  # 宽容期结束只判定一次。
  if (( in_grace )) && (( SECONDS - script_start >= STARTUP_GRACE )); then
    in_grace=0
    log info "启动宽容期结束,开始计入失败计数"
  fi

  # 达到阈值 → 触发重启。即使在 grace 期,达到阈值也重启
  # (grace 是"不计入计数",不是"禁止重启")。
  if (( fail_count >= MAX_FAILURES )); then
    if (( restarts_in_window >= MAX_RESTARTS )); then
      log error "重启次数达到上限 $MAX_RESTARTS / ${RESTART_WINDOW}s — 放弃 (可能是配置错误或依赖故障)"
      exit 2
    fi

    log error "连续失败 $fail_count 次,触发重启脚本: $RESTART_SCRIPT"
    # 不 set -e,所以重启脚本的非零退出码不会让我们自己退出。
    # 记录重启脚本的退出码,无论成功失败都进入冷却期——重启脚本可能
    # 是异步发信号,非零不代表"没重启成功"。
    start_secs=$SECONDS
    if "$RESTART_SCRIPT"; then
      log info "重启脚本退出 0 (耗时 $((SECONDS - start_secs))s)"
    else
      rc=$?
      log error "重启脚本退出 $rc (耗时 $((SECONDS - start_secs))s)"
    fi

    restarts_in_window=$((restarts_in_window + 1))
    fail_count=0

    log info "冷却 ${RESTART_WAIT}s 后恢复探测"
    sleep "$RESTART_WAIT"

    # 冷却后重新进入宽容期,等网关真正起来再开始计数。
    in_grace=1
    script_start=$SECONDS
    continue
  fi

  sleep "$INTERVAL"
done
