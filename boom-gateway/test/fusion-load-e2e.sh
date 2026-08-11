#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
GATEWAY_ROOT=$(cd "${SCRIPT_DIR}/../.." && pwd)

PROFILE=${FUSION_LOAD_PROFILE:-release}
SKIP_BUILD=${FUSION_LOAD_SKIP_BUILD:-0}
OUTPUT_DIR=${FUSION_LOAD_OUTPUT_DIR:-"/tmp/boom-fusion-readme-$(date -u +%Y%m%dT%H%M%SZ)-$$"}
WORK_DIR=$(mktemp -d "/tmp/boom-fusion-readme-work-XXXXXX")

BASELINE_QPS=${FUSION_LOAD_BASELINE_QPS:-1000}
BASELINE_DURATION=${FUSION_LOAD_BASELINE_DURATION:-120s}

PROTOCOL_QPS=${FUSION_LOAD_PROTOCOL_QPS:-500}
PROTOCOL_DURATION=${FUSION_LOAD_PROTOCOL_DURATION:-60s}

KEY_COUNT=${FUSION_LOAD_KEY_COUNT:-100}
MULTI_KEY_CONCURRENCY=${FUSION_LOAD_MULTI_KEY_CONCURRENCY:-2000}
MULTI_KEY_DURATION=${FUSION_LOAD_MULTI_KEY_DURATION:-120s}

PROMPT_QPS=${FUSION_LOAD_PROMPT_QPS:-200}
PROMPT_DURATION=${FUSION_LOAD_PROMPT_DURATION:-60s}
SHORT_PROMPT_CHARS=${FUSION_LOAD_SHORT_PROMPT_CHARS:-1000}
LONG_PROMPT_CHARS=${FUSION_LOAD_LONG_PROMPT_CHARS:-100000}

RAMP_MODE=${FUSION_LOAD_RAMP_MODE:-ramp=100,5000,500,30}
RAMP_DURATION=${FUSION_LOAD_RAMP_DURATION:-15m}

REQUEST_TIMEOUT_SECS=${FUSION_LOAD_REQUEST_TIMEOUT_SECS:-120}
GATEWAY_RUST_LOG=${FUSION_LOAD_GATEWAY_RUST_LOG:-warn}
START_RETRIES=${FUSION_LOAD_START_RETRIES:-5}

MASTER_KEY="fusion-load-master-key"
RUN_ID="$(date -u +%Y%m%dT%H%M%SZ)-$$"
POSTGRES_CONTAINER=""
PIDS=()
REPORTS=()

mkdir -p "${OUTPUT_DIR}/logs" "${OUTPUT_DIR}/reports" "${OUTPUT_DIR}/stats"

log() {
    printf '[fusion-load] %s\n' "$*"
}

fail() {
    printf '[fusion-load] ERROR: %s\n' "$*" >&2
    exit 1
}

cleanup() {
    local pid
    set +e
    for pid in "${PIDS[@]:-}"; do
        kill "${pid}" >/dev/null 2>&1 || true
    done
    for pid in "${PIDS[@]:-}"; do
        wait "${pid}" >/dev/null 2>&1 || true
    done
    if [[ -n "${POSTGRES_CONTAINER}" ]]; then
        docker logs "${POSTGRES_CONTAINER}" \
            >"${OUTPUT_DIR}/logs/postgres.log" 2>&1 || true
        docker stop --time 5 "${POSTGRES_CONTAINER}" >/dev/null 2>&1 || true
        docker rm "${POSTGRES_CONTAINER}" >/dev/null 2>&1 || true
    fi
    rm -rf "${WORK_DIR}"
}

trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

require_command() {
    command -v "$1" >/dev/null 2>&1 || fail "required command not found: $1"
}

is_positive_integer() {
    [[ "$1" =~ ^[1-9][0-9]*$ ]]
}

duration_to_seconds() {
    local value=$1
    case "${value}" in
        *ms)
            local milliseconds=${value%ms}
            is_positive_integer "${milliseconds}" || fail "invalid duration: ${value}"
            printf '%s\n' "$(((milliseconds + 999) / 1000))"
            ;;
        *s)
            local seconds=${value%s}
            is_positive_integer "${seconds}" || fail "invalid duration: ${value}"
            printf '%s\n' "${seconds}"
            ;;
        *m)
            local minutes=${value%m}
            is_positive_integer "${minutes}" || fail "invalid duration: ${value}"
            printf '%s\n' "$((minutes * 60))"
            ;;
        *)
            is_positive_integer "${value}" || fail "invalid duration: ${value}"
            printf '%s\n' "${value}"
            ;;
    esac
}

pick_port() {
    local port
    while true; do
        port=$(shuf -i 20000-60000 -n 1)
        if ! (exec 3<>"/dev/tcp/127.0.0.1/${port}") 2>/dev/null; then
            printf '%s\n' "${port}"
            return
        fi
    done
}

start_process() {
    local name=$1
    local output=$2
    shift 2
    "$@" >"${output}" 2>&1 &
    local pid=$!
    PIDS+=("${pid}")
    printf -v "${name}" '%s' "${pid}"
}

stop_process() {
    local pid=$1
    kill "${pid}" >/dev/null 2>&1 || true
    wait "${pid}" >/dev/null 2>&1 || true
}

wait_for_http() {
    local url=$1
    local pid=$2
    local attempt
    for attempt in $(seq 1 300); do
        if ! kill -0 "${pid}" >/dev/null 2>&1; then
            return 1
        fi
        if curl -fsS --max-time 1 "${url}" >/dev/null 2>&1; then
            return 0
        fi
        sleep 0.1
    done
    return 1
}

wait_for_backend_idle() {
    local name=$1
    local url=$2
    local attempt
    for attempt in $(seq 1 300); do
        if [[ "$(curl -fsS "${url}/internal/stats" | jq -r '.inflight')" == "0" ]]; then
            return
        fi
        sleep 0.1
    done
    fail "${name} still has inflight requests after 30 seconds"
}

validate_settings() {
    case "${PROFILE}" in
        release|debug) ;;
        *) fail "FUSION_LOAD_PROFILE must be release or debug" ;;
    esac

    local number
    for number in \
        "${BASELINE_QPS}" \
        "${PROTOCOL_QPS}" \
        "${KEY_COUNT}" \
        "${MULTI_KEY_CONCURRENCY}" \
        "${PROMPT_QPS}" \
        "${SHORT_PROMPT_CHARS}" \
        "${LONG_PROMPT_CHARS}" \
        "${REQUEST_TIMEOUT_SECS}" \
        "${START_RETRIES}"; do
        is_positive_integer "${number}" || fail "expected a positive integer, got: ${number}"
    done

    duration_to_seconds "${BASELINE_DURATION}" >/dev/null
    duration_to_seconds "${PROTOCOL_DURATION}" >/dev/null
    duration_to_seconds "${MULTI_KEY_DURATION}" >/dev/null
    duration_to_seconds "${PROMPT_DURATION}" >/dev/null
    duration_to_seconds "${RAMP_DURATION}" >/dev/null

    local ramp_values=${RAMP_MODE#ramp=}
    [[ "${ramp_values}" != "${RAMP_MODE}" ]] || fail "ramp mode must be ramp=FROM,TO,STEP,DURATION_SECS"
    local ramp_from ramp_to ramp_step ramp_step_seconds extra
    IFS=, read -r ramp_from ramp_to ramp_step ramp_step_seconds extra <<<"${ramp_values}"
    [[ -z "${extra:-}" ]] || fail "invalid ramp mode: ${RAMP_MODE}"
    for number in "${ramp_from}" "${ramp_to}" "${ramp_step}" "${ramp_step_seconds}"; do
        is_positive_integer "${number}" || fail "invalid ramp mode: ${RAMP_MODE}"
    done
    ((ramp_from <= ramp_to)) || fail "ramp FROM must not exceed TO"
}

build_binaries() {
    if [[ "${SKIP_BUILD}" == "1" ]]; then
        log "skipping builds because FUSION_LOAD_SKIP_BUILD=1"
        return
    fi

    log "building real boom-gateway (${PROFILE})"
    if [[ "${PROFILE}" == "release" ]]; then
        cargo build --manifest-path "${GATEWAY_ROOT}/Cargo.toml" \
            --release -p boom-main --bin boom-gateway
        cargo build --manifest-path "${SCRIPT_DIR}/mock-backend/Cargo.toml" --release
        cargo build --manifest-path "${SCRIPT_DIR}/bench-client/Cargo.toml" --release
    else
        cargo build --manifest-path "${GATEWAY_ROOT}/Cargo.toml" \
            -p boom-main --bin boom-gateway
        cargo build --manifest-path "${SCRIPT_DIR}/mock-backend/Cargo.toml"
        cargo build --manifest-path "${SCRIPT_DIR}/bench-client/Cargo.toml"
    fi
}

start_postgres() {
    if [[ -n "${FUSION_LOAD_DATABASE_URL:-}" ]]; then
        DATABASE_URL=${FUSION_LOAD_DATABASE_URL}
        log "using FUSION_LOAD_DATABASE_URL; the database must be disposable"
        return
    fi

    POSTGRES_CONTAINER="boom-fusion-load-${RUN_ID}"
    log "starting temporary PostgreSQL container ${POSTGRES_CONTAINER}"
    docker run -d \
        --name "${POSTGRES_CONTAINER}" \
        -e POSTGRES_USER=fusion_load \
        -e POSTGRES_PASSWORD=fusion_load \
        -e POSTGRES_DB=fusion_load \
        -p 127.0.0.1::5432 \
        postgres:16-alpine >/dev/null

    local attempt ready_streak=0
    for attempt in $(seq 1 600); do
        if docker exec "${POSTGRES_CONTAINER}" \
            pg_isready -U fusion_load -d fusion_load >/dev/null 2>&1; then
            ready_streak=$((ready_streak + 1))
            if ((ready_streak >= 10)); then
                break
            fi
        else
            ready_streak=0
        fi
        if [[ "$(docker inspect -f '{{.State.Running}}' "${POSTGRES_CONTAINER}" 2>/dev/null)" != "true" ]]; then
            break
        fi
        sleep 0.2
    done
    if ((ready_streak < 10)); then
        docker logs "${POSTGRES_CONTAINER}" \
            >"${OUTPUT_DIR}/logs/postgres.log" 2>&1 || true
        fail "temporary PostgreSQL did not become stably ready; see ${OUTPUT_DIR}/logs/postgres.log"
    fi

    local binding
    binding=$(docker port "${POSTGRES_CONTAINER}" 5432/tcp)
    local postgres_port=${binding##*:}
    is_positive_integer "${postgres_port}" \
        || fail "could not determine PostgreSQL host port from: ${binding}"
    DATABASE_URL="postgres://fusion_load:fusion_load@127.0.0.1:${postgres_port}/fusion_load"
}

start_mock() {
    local pid_name=$1
    local port_name=$2
    local log_name=$3
    shift 3
    local attempt port pid log_path
    log_path="${OUTPUT_DIR}/logs/${log_name}.log"

    for attempt in $(seq 1 "${START_RETRIES}"); do
        port=$(pick_port)
        printf -v "${port_name}" '%s' "${port}"
        start_process "${pid_name}" "${log_path}" \
            env RUST_LOG=warn "${MOCK_BIN}" \
            --bind "127.0.0.1:${port}" \
            --min-chars 100 \
            --max-chars 400 \
            --chunk-interval-ms 2 \
            --max-concurrent 10000 \
            --reject-empty-tools \
            "$@"
        pid=${!pid_name}
        if wait_for_http "http://127.0.0.1:${port}/health" "${pid}"; then
            return
        fi
        stop_process "${pid}"
        mv "${log_path}" "${OUTPUT_DIR}/logs/${log_name}-start-${attempt}.log"
        log "${log_name} failed to start on port ${port}; retrying"
    done

    fail "${log_name} failed to start after ${START_RETRIES} attempts; see ${OUTPUT_DIR}/logs"
}

start_gateway() {
    local attempt pid log_path="${OUTPUT_DIR}/logs/gateway.log"

    for attempt in $(seq 1 "${START_RETRIES}"); do
        GATEWAY_PORT=$(pick_port)
        GATEWAY_URL="http://127.0.0.1:${GATEWAY_PORT}"
        start_process GATEWAY_PID "${log_path}" \
            env FUSION_LOAD_DB_URL="${DATABASE_URL}" RUST_LOG="${GATEWAY_RUST_LOG}" \
            "${GATEWAY_BIN}" \
            --config "${WORK_DIR}/config.yaml" \
            --host 127.0.0.1 \
            --port "${GATEWAY_PORT}"
        pid=${GATEWAY_PID}
        if wait_for_http "${GATEWAY_URL}/health" "${pid}"; then
            return
        fi
        stop_process "${pid}"
        mv "${log_path}" "${OUTPUT_DIR}/logs/gateway-start-${attempt}.log"
        log "gateway failed to start on port ${GATEWAY_PORT}; retrying"
    done

    fail "gateway failed to start after ${START_RETRIES} attempts; see ${OUTPUT_DIR}/logs"
}

write_gateway_config() {
    cat >"${WORK_DIR}/config.yaml" <<EOF
model_list:
  - model_name: panel-a
    model_info:
      id: panel-a-load-deployment
    litellm_params:
      model: openai/panel-a-upstream
      api_key: mock-key
      api_base: http://127.0.0.1:${PANEL_A_PORT}/v1
      timeout: 1200
  - model_name: panel-b
    model_info:
      id: panel-b-load-deployment
    litellm_params:
      model: openai/panel-b-upstream
      api_key: mock-key
      api_base: http://127.0.0.1:${PANEL_B_PORT}/v1
      timeout: 1200
  - model_name: aggregator
    model_info:
      id: aggregator-load-deployment
    litellm_params:
      model: openai/aggregator-upstream
      api_key: mock-key
      api_base: http://127.0.0.1:${AGGREGATOR_PORT}/v1
      timeout: 1200
  - model_name: mock-gpt-4o
    model_info:
      id: protocol-openai-load-deployment
    litellm_params:
      model: openai/mock-gpt-4o
      api_key: mock-key
      api_base: http://127.0.0.1:${PROTOCOL_PORT}/v1
      timeout: 1200
  - model_name: mock-claude
    model_info:
      id: protocol-anthropic-load-deployment
    litellm_params:
      model: openai/mock-claude
      api_key: mock-key
      api_base: http://127.0.0.1:${PROTOCOL_PORT}/v1
      timeout: 1200

workflow_settings:
  models:
    fusion: direct_synthesis
  workflows:
    direct_synthesis:
      type: direct_synthesis
      roles:
        panel:
          - model: panel-a
            temperature: 0.3
          - model: panel-b
            temperature: 0.5
        aggregator:
          model: aggregator
          temperature: 0

general_settings:
  master_key: ${MASTER_KEY}
  database_url: \${FUSION_LOAD_DB_URL}

router_settings:
  routing_strategy: round_robin

rate_limit:
  enabled: false

deployment_health_check:
  auto_offline_enabled: false
  auto_recovery_enabled: false
  request_failure_auto_offline_enabled: false
EOF
}

snapshot_stats() {
    local case_name=$1
    local stage=$2
    curl -fsS "${PANEL_A_URL}/internal/stats" \
        >"${OUTPUT_DIR}/stats/${case_name}-${stage}-panel-a.json"
    curl -fsS "${PANEL_B_URL}/internal/stats" \
        >"${OUTPUT_DIR}/stats/${case_name}-${stage}-panel-b.json"
    curl -fsS "${AGGREGATOR_URL}/internal/stats" \
        >"${OUTPUT_DIR}/stats/${case_name}-${stage}-aggregator.json"
    curl -fsS "${PROTOCOL_URL}/internal/stats" \
        >"${OUTPUT_DIR}/stats/${case_name}-${stage}-protocol.json"
}

stats_delta() {
    local case_name=$1
    local backend=$2
    local field=$3
    local before="${OUTPUT_DIR}/stats/${case_name}-before-${backend}.json"
    local after="${OUTPUT_DIR}/stats/${case_name}-after-${backend}.json"
    jq -n \
        --argjson before "$(jq ".${field}" "${before}")" \
        --argjson after "$(jq ".${field}" "${after}")" \
        '$after - $before'
}

assert_backend_stats() {
    local case_name=$1
    local topology=$2
    local policy=$3
    local sent=$4
    local backend expected actual field

    for backend in panel-a panel-b aggregator protocol; do
        for field in rejected_model rejected_empty_tools; do
            actual=$(stats_delta "${case_name}" "${backend}" "${field}")
            [[ "${actual}" == "0" ]] \
                || fail "${case_name}: ${backend}.${field} increased by ${actual}"
        done
        if [[ "${policy}" == "strict" ]]; then
            actual=$(stats_delta "${case_name}" "${backend}" rejected_503)
            [[ "${actual}" == "0" ]] \
                || fail "${case_name}: ${backend}.rejected_503 increased by ${actual}"
        fi

        actual=$(jq -r '.inflight' \
            "${OUTPUT_DIR}/stats/${case_name}-after-${backend}.json")
        [[ "${actual}" == "0" ]] \
            || fail "${case_name}: ${backend} still has inflight=${actual}"
    done

    if [[ "${policy}" == "ramp" ]]; then
        local panel_a panel_b aggregator protocol
        panel_a=$(stats_delta "${case_name}" panel-a total_received)
        panel_b=$(stats_delta "${case_name}" panel-b total_received)
        aggregator=$(stats_delta "${case_name}" aggregator total_received)
        protocol=$(stats_delta "${case_name}" protocol total_received)

        [[ "${panel_a}" == "${panel_b}" ]] \
            || fail "${case_name}: panel receive counts differ: ${panel_a} vs ${panel_b}"
        ((panel_a >= sent && panel_a <= sent * 2)) \
            || fail "${case_name}: panel calls ${panel_a} outside expected range ${sent}..$((sent * 2))"
        ((aggregator >= 0 && aggregator <= sent)) \
            || fail "${case_name}: aggregator calls ${aggregator} exceed sent=${sent}"
        [[ "${protocol}" == "0" ]] \
            || fail "${case_name}: protocol backend unexpectedly received ${protocol}"
        return
    fi

    for backend in panel-a panel-b aggregator protocol; do
        case "${topology}:${backend}" in
            fusion:panel-a|fusion:panel-b|fusion:aggregator)
                expected=${sent}
                ;;
            protocol:protocol)
                expected=${sent}
                ;;
            *)
                expected=0
                ;;
        esac
        actual=$(stats_delta "${case_name}" "${backend}" total_received)
        [[ "${actual}" == "${expected}" ]] \
            || fail "${case_name}: ${backend} received ${actual}, expected ${expected}"
    done
}

assert_report() {
    local case_name=$1
    local report=$2
    local expected_keys=$3
    local policy=$4

    jq -e \
        --argjson expected_keys "${expected_keys}" \
        --arg policy "${policy}" \
        '
        (.err_429 + .err_5xx + .err_4xx + .err_timeout
          + .err_connect + .err_parse + .err_stream) as $errors
        |
        .sent > 0
        and .sent == (.ok + $errors)
        and (
          $policy == "ramp"
          or (
            .ok == .sent
            and $errors == 0
          )
        )
        and .ttft.count == .ok
        and .e2e.count == .ok
        and .keys_configured == $expected_keys
        and .keys_used == $expected_keys
        and .key_requests_min > 0
        ' "${report}" >/dev/null \
        || fail "${case_name}: report assertions failed; see ${report}"
}

write_backend_delta() {
    local case_name=$1
    local backend
    local delta_dir="${WORK_DIR}/${case_name}-backend-delta"
    mkdir -p "${delta_dir}"

    for backend in panel-a panel-b aggregator protocol; do
        jq -n \
            --arg backend "${backend}" \
            --argjson total_received "$(stats_delta "${case_name}" "${backend}" total_received)" \
            --argjson rejected_503 "$(stats_delta "${case_name}" "${backend}" rejected_503)" \
            --argjson rejected_model "$(stats_delta "${case_name}" "${backend}" rejected_model)" \
            --argjson rejected_empty_tools "$(stats_delta "${case_name}" "${backend}" rejected_empty_tools)" \
            '{
              backend: $backend,
              total_received: $total_received,
              rejected_503: $rejected_503,
              rejected_model: $rejected_model,
              rejected_empty_tools: $rejected_empty_tools
            }' >"${delta_dir}/${backend}.json"
    done

    jq -s \
        'map({key: .backend, value: del(.backend)}) | from_entries' \
        "${delta_dir}"/*.json \
        >"${OUTPUT_DIR}/stats/${case_name}-delta.json"
}

run_case() {
    local case_name=$1
    local format=$2
    local auth_style=$3
    local keys=$4
    local expected_keys=$5
    local model=$6
    local prompt_min=$7
    local prompt_max=$8
    local mode=$9
    local duration=${10}
    local stream=${11}
    local topology=${12}
    local policy=${13}

    local report="${OUTPUT_DIR}/reports/${case_name}.json"
    local output="${OUTPUT_DIR}/logs/${case_name}.out"
    local exit_code

    log "running ${case_name}: format=${format} model=${model} mode=${mode} duration=${duration} stream=${stream}"
    snapshot_stats "${case_name}" before

    set +e
    RUST_LOG=bench_client=info "${BENCH_BIN}" \
        --target "${GATEWAY_URL}" \
        --format "${format}" \
        --auth-style "${auth_style}" \
        --keys "${keys}" \
        --model "${model}" \
        --prompt-min "${prompt_min}" \
        --prompt-max "${prompt_max}" \
        --mode "${mode}" \
        --duration "${duration}" \
        --stream="${stream}" \
        --request-timeout-secs "${REQUEST_TIMEOUT_SECS}" \
        --report "${report}" 2>&1 | tee "${output}"
    exit_code=${PIPESTATUS[0]}
    set -e

    [[ "${exit_code}" == "0" ]] \
        || fail "${case_name}: bench-client exited with ${exit_code}; see ${output}"
    [[ -s "${report}" ]] || fail "${case_name}: report was not written"
    assert_report "${case_name}" "${report}" "${expected_keys}" "${policy}"

    wait_for_backend_idle panel-a "${PANEL_A_URL}"
    wait_for_backend_idle panel-b "${PANEL_B_URL}"
    wait_for_backend_idle aggregator "${AGGREGATOR_URL}"
    wait_for_backend_idle protocol "${PROTOCOL_URL}"
    snapshot_stats "${case_name}" after
    write_backend_delta "${case_name}"

    local sent
    sent=$(jq -r '.sent' "${report}")
    assert_backend_stats "${case_name}" "${topology}" "${policy}" "${sent}"
    REPORTS+=("${report}")
    log "${case_name} completed: ok=$(jq -r '.ok' "${report}"), success_qps=$(jq -r '.success_qps' "${report}")"
    if [[ "${policy}" == "ramp" ]]; then
        log "${case_name} backend overload counts: $(jq -c \
            'with_entries(.value |= .rejected_503)' \
            "${OUTPUT_DIR}/stats/${case_name}-delta.json")"
    fi
}

create_virtual_keys() {
    local request_file="${WORK_DIR}/batch-keys-request.json"
    local response_file="${WORK_DIR}/batch-keys-response.json"
    local cookie_file="${WORK_DIR}/admin-cookie.txt"

    curl -fsS -c "${cookie_file}" \
        -H "Content-Type: application/json" \
        -d "{\"user_id\":\"admin\",\"api_key\":\"${MASTER_KEY}\"}" \
        "${GATEWAY_URL}/dashboard/api/auth/login" >/dev/null

    jq -n \
        --arg run_id "${RUN_ID}" \
        --argjson count "${KEY_COUNT}" \
        '[
          range(1; $count + 1) |
          {
            key_alias: ("fusion-load-" + $run_id + "-" + (. | tostring)),
            key_name: ("Fusion load key " + (. | tostring)),
            models: ["fusion"]
          }
        ]' >"${request_file}"

    curl -fsS -b "${cookie_file}" \
        -H "Content-Type: application/json" \
        --data-binary "@${request_file}" \
        "${GATEWAY_URL}/dashboard/api/admin/keys/batch" >"${response_file}"

    jq -e \
        --argjson expected "${KEY_COUNT}" \
        '.created_count == $expected and .skipped_count == 0' \
        "${response_file}" >/dev/null \
        || fail "Dashboard batch key creation did not create exactly ${KEY_COUNT} keys"

    VIRTUAL_KEYS=$(jq -r '[.created[].key] | join(",")' "${response_file}")
    [[ -n "${VIRTUAL_KEYS}" ]] || fail "Dashboard batch key response did not contain raw keys"
    log "created ${KEY_COUNT} virtual keys through the Dashboard API"
}

assert_ramp_steps() {
    local output="${OUTPUT_DIR}/logs/E-ramp.out"
    local duration_seconds
    duration_seconds=$(duration_to_seconds "${RAMP_DURATION}")

    local ramp_values=${RAMP_MODE#ramp=}
    local ramp_from ramp_to ramp_step ramp_step_seconds
    IFS=, read -r ramp_from ramp_to ramp_step ramp_step_seconds <<<"${ramp_values}"

    local expected_steps=$(((duration_seconds + ramp_step_seconds - 1) / ramp_step_seconds))
    local actual_steps
    actual_steps=$(rg -c 'ramp step: qps=' "${output}" || true)
    [[ "${actual_steps}" == "${expected_steps}" ]] \
        || fail "E-ramp: observed ${actual_steps} ramp steps, expected ${expected_steps}"

    local steps_to_target=$((((ramp_to - ramp_from) + ramp_step - 1) / ramp_step + 1))
    if ((expected_steps >= steps_to_target)); then
        local expected_target_steps=$((expected_steps - steps_to_target + 1))
        local actual_target_steps
        actual_target_steps=$(rg -c "ramp step: qps=${ramp_to} " "${output}" || true)
        [[ "${actual_target_steps}" == "${expected_target_steps}" ]] \
            || fail "E-ramp: target qps=${ramp_to} held for ${actual_target_steps} steps, expected ${expected_target_steps}"
    fi
    log "E-ramp step sequence passed: ${actual_steps} steps"
}

write_summary() {
    jq -s \
        '{
          cases: length,
          total_sent: (map(.sent) | add),
          total_ok: (map(.ok) | add),
          total_errors: (
            map(
              .err_429 + .err_5xx + .err_4xx + .err_timeout
              + .err_connect + .err_parse + .err_stream
            ) | add
          ),
          reports: .
        }' "${REPORTS[@]}" >"${OUTPUT_DIR}/summary.json"
}

require_command cargo
require_command curl
require_command docker
require_command jq
require_command rg
require_command shuf
validate_settings
build_binaries

PROFILE_DIR=${PROFILE}
GATEWAY_BIN="${GATEWAY_ROOT}/target/${PROFILE_DIR}/boom-gateway"
MOCK_BIN="${SCRIPT_DIR}/mock-backend/target/${PROFILE_DIR}/mock-backend"
BENCH_BIN="${SCRIPT_DIR}/bench-client/target/${PROFILE_DIR}/bench-client"
[[ -x "${GATEWAY_BIN}" ]] || fail "gateway binary not found: ${GATEWAY_BIN}"
[[ -x "${MOCK_BIN}" ]] || fail "mock binary not found: ${MOCK_BIN}"
[[ -x "${BENCH_BIN}" ]] || fail "bench binary not found: ${BENCH_BIN}"

start_mock PANEL_A_PID PANEL_A_PORT panel-a \
    --served-model panel-a-upstream
start_mock PANEL_B_PID PANEL_B_PORT panel-b \
    --served-model panel-b-upstream
start_mock AGGREGATOR_PID AGGREGATOR_PORT aggregator \
    --served-model aggregator-upstream
start_mock PROTOCOL_PID PROTOCOL_PORT protocol

PANEL_A_URL="http://127.0.0.1:${PANEL_A_PORT}"
PANEL_B_URL="http://127.0.0.1:${PANEL_B_PORT}"
AGGREGATOR_URL="http://127.0.0.1:${AGGREGATOR_PORT}"
PROTOCOL_URL="http://127.0.0.1:${PROTOCOL_PORT}"

start_postgres
write_gateway_config
start_gateway

create_virtual_keys

run_case \
    A-baseline \
    openai bearer "${MASTER_KEY}" 1 fusion \
    1000 5000 "qps=${BASELINE_QPS}" "${BASELINE_DURATION}" true fusion strict

run_case \
    B-openai \
    openai bearer "${MASTER_KEY}" 1 mock-gpt-4o \
    2000 2000 "qps=${PROTOCOL_QPS}" "${PROTOCOL_DURATION}" true protocol strict

run_case \
    B-anthropic \
    anthropic anthropic "${MASTER_KEY}" 1 mock-claude \
    2000 2000 "qps=${PROTOCOL_QPS}" "${PROTOCOL_DURATION}" true protocol strict

run_case \
    C-multi-key \
    openai bearer "${VIRTUAL_KEYS}" "${KEY_COUNT}" fusion \
    2000 2000 "concurrent=${MULTI_KEY_CONCURRENCY}" "${MULTI_KEY_DURATION}" false fusion strict

run_case \
    D-short-prompt \
    openai bearer "${MASTER_KEY}" 1 fusion \
    "${SHORT_PROMPT_CHARS}" "${SHORT_PROMPT_CHARS}" \
    "qps=${PROMPT_QPS}" "${PROMPT_DURATION}" true fusion strict

run_case \
    D-long-prompt \
    openai bearer "${MASTER_KEY}" 1 fusion \
    "${LONG_PROMPT_CHARS}" "${LONG_PROMPT_CHARS}" \
    "qps=${PROMPT_QPS}" "${PROMPT_DURATION}" true fusion strict

run_case \
    E-ramp \
    openai bearer "${MASTER_KEY}" 1 fusion \
    1000 5000 "${RAMP_MODE}" "${RAMP_DURATION}" true fusion ramp
assert_ramp_steps

write_summary
log "all README load scenarios completed; A-D functional assertions passed"
log "reports and process logs: ${OUTPUT_DIR}"
