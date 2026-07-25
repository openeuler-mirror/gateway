#!/usr/bin/env bash
#
# 批量创建 boom-gateway 测试 key（不限流）—— 纯 bash，零外部依赖
#
# 用法：
#   MASTER_KEY=sk-xxx ./scripts/create_keys.sh <数量> [网网关地址]
#
# 参数：
#   $1  数量       必填
#   $2  网关地址   可选，默认 http://localhost:8080
#
# 环境变量：
#   MASTER_KEY        必填，config.yaml 里的 master_key
#   KEY_ALIAS_PREFIX  可选，别名前缀，默认 test-key-
#   KEY_USER_ID       可选，user_id 字段，默认 test
#
# 输出（stdout）：
#   created: N, skipped: M
#   （空行）
#   每行：明文key<Tab>别名
#   （空行，仅 skipped > 0 时显示 skipped 详情）
#
# 退出码：0 成功 | 1 参数错误 | 2 登录失败 | 3 接口错误
#
# 示例：
#   MASTER_KEY=sk-xxx ./scripts/create_keys.sh 10
#   MASTER_KEY=sk-xxx ./scripts/create_keys.sh 5 http://1.2.3.4:8080 > keys.txt

# 不用 set -e，避免 grep/计数的非零退出码提前终止脚本
set -uo pipefail

# ── 参数校验 ──────────────────────────────────────────────
if [[ $# -lt 1 ]]; then
  echo "用法: MASTER_KEY=sk-xxx $0 <数量> [网关地址]" >&2
  exit 1
fi

N="$1"
GATEWAY="${2:-http://localhost:8080}"
MASTER_KEY="${MASTER_KEY:-}"
ALIAS_PREFIX="${KEY_ALIAS_PREFIX:-test-key-}"
USER_ID="${KEY_USER_ID:-test}"

if [[ -z "$MASTER_KEY" ]]; then
  echo "错误: 未设 MASTER_KEY 环境变量" >&2
  exit 1
fi

if ! [[ "$N" =~ ^[1-9][0-9]*$ ]]; then
  echo "错误: 数量必须是正整数，收到: $N" >&2
  exit 1
fi

command -v curl >/dev/null || { echo "错误: 缺少 curl" >&2; exit 1; }

# ── 1. 登录拿 cookie ───────────────────────────────────────
COOKIE_JAR="$(mktemp)"
trap 'rm -f "$COOKIE_JAR"' EXIT

echo "→ 登录 $GATEWAY ..." >&2
LOGIN_RESP=$(curl -sS -X POST "$GATEWAY/dashboard/api/auth/login" \
  -H "Content-Type: application/json" \
  -c "$COOKIE_JAR" \
  -d "{\"user_id\":\"admin\",\"api_key\":\"$MASTER_KEY\"}" 2>&1)
LOGIN_RC=$?

if [[ $LOGIN_RC -ne 0 || ! -s "$COOKIE_JAR" ]]; then
  echo "错误: 登录失败（master_key 错或网关不通）" >&2
  echo "响应: $LOGIN_RESP" >&2
  exit 2
fi
echo "✓ 登录成功" >&2

# ── 2. 构造 payload ─────────────────────────────────────────
# 每个 item 只填 key_alias + user_id + metadata 三个字段
# 不传 plan_name/rpm_limit/tpm_limit = 不限流
ITEMS=""
for i in $(seq 1 "$N"); do
  ALIAS="${ALIAS_PREFIX}${i}"
  ITEM="    {\"key_alias\":\"${ALIAS}\",\"user_id\":\"${USER_ID}\",\"metadata\":{\"env\":\"qa\"}}"
  if [[ -z "$ITEMS" ]]; then
    ITEMS="$ITEM"
  else
    ITEMS="$ITEMS,
$ITEM"
  fi
done
PAYLOAD="[$ITEMS]"

# ── 3. 调批量接口 ──────────────────────────────────────────
echo "→ 创建 $N 个 key..." >&2
RESP=$(curl -sS -X POST "$GATEWAY/dashboard/api/admin/keys/batch" \
  -H "Content-Type: application/json" \
  -b "$COOKIE_JAR" \
  -d "$PAYLOAD" 2>&1)
CURL_RC=$?

if [[ $CURL_RC -ne 0 ]]; then
  echo "错误: 批量创建请求失败" >&2
  echo "响应: $RESP" >&2
  exit 3
fi

# 检查响应里是否有 error 字段
if echo "$RESP" | grep -q '"error"'; then
  ERR=$(echo "$RESP" | sed -n 's/.*"error"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')
  echo "错误: 接口返回: $ERR" >&2
  exit 3
fi

# ── 4. 解析响应并输出 ────────────────────────────────────────
# 响应: {"created":[{"key":"sk-xxx","key_alias":"test-key-1"},...],"skipped":[...]}

CREATED_KEYS=$(echo "$RESP" | grep -o '"key"[[:space:]]*:[[:space:]]*"[^"]*"' | sed 's/.*"key"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/')
CREATED_ALIASES=$(echo "$RESP" | grep -o '"key_alias"[[:space:]]*:[[:space:]]*"[^"]*"' | sed 's/.*"key_alias"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/')

# grep -c 在空输入时退出码是 1，加 || true 避免脚本中断
CREATED_COUNT=$(echo "$CREATED_KEYS" | grep -c . || true)
SKIPPED_COUNT=$(echo "$RESP" | grep -o '"reason"' | grep -c . || true)

# 空字符串变量 grep -c . 返回 0，但有些系统行为不同，强制修正
[[ -z "$CREATED_KEYS" ]] && CREATED_COUNT=0
[[ -z "$SKIPPED_COUNT" ]] && SKIPPED_COUNT=0

echo "created: $CREATED_COUNT, skipped: $SKIPPED_COUNT"
echo

# 输出 key<Tab>alias 列表（用 paste 把两列按行对齐）
if [[ "$CREATED_COUNT" -gt 0 ]]; then
  echo "$CREATED_KEYS" | paste - <(echo "$CREATED_ALIASES")
fi

echo

# skipped 详情
if [[ "$SKIPPED_COUNT" -gt 0 ]]; then
  echo "skipped:"
  echo "$RESP" | sed -n 's/.*"skipped"[[:space:]]*:[[:space:]]*\[\(.*\)\].*/\1/p' | tr ',' '\n' | sed 's/^/  /'
fi

echo "✓ 完成" >&2
