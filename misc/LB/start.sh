#!/bin/bash
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
IMAGE_NAME="gateway-lb"
CONTAINER_NAME="gateway-lb"

# Parse arguments
ACTION="${1:-start}"
CONFIG_FILE="${2:-$SCRIPT_DIR/config.yaml}"

usage() {
    echo "Usage: $0 {start|stop|restart|status} [config_file]"
    echo ""
    echo "Commands:"
    echo "  start [config]   Build image if needed, generate cert, start container"
    echo "  stop             Stop and remove container"
    echo "  restart [config] Stop then start"
    echo "  status           Show container status"
    echo ""
    echo "Default config: $SCRIPT_DIR/config.yaml"
    exit 1
}

do_stop() {
    if docker ps -a --format '{{.Names}}' | grep -q "^${CONTAINER_NAME}$"; then
        echo "Stopping ${CONTAINER_NAME}..."
        docker stop "$CONTAINER_NAME" 2>/dev/null || true
        docker rm "$CONTAINER_NAME" 2>/dev/null || true
        echo "Container removed."
    else
        echo "Container ${CONTAINER_NAME} not running."
    fi
}

do_status() {
    if docker ps --format '{{.Names}}' | grep -q "^${CONTAINER_NAME}$"; then
        echo "${CONTAINER_NAME} is running:"
        docker ps --filter "name=^${CONTAINER_NAME}$" --format "table {{.Names}}\t{{.Status}}\t{{.Ports}}"
    elif docker ps -a --format '{{.Names}}' | grep -q "^${CONTAINER_NAME}$"; then
        echo "${CONTAINER_NAME} is stopped."
    else
        echo "${CONTAINER_NAME} does not exist."
    fi
}

generate_cert() {
    local cert_dir="$SCRIPT_DIR/certs"
    if [ -f "$cert_dir/server.crt" ] && [ -f "$cert_dir/server.key" ]; then
        echo "TLS certificate already exists, skipping generation."
        return
    fi
    echo "Generating self-signed TLS certificate..."
    mkdir -p "$cert_dir"
    openssl req -x509 -newkey rsa:2048 -sha256 \
        -days 3650 -nodes \
        -keyout "$cert_dir/server.key" \
        -out "$cert_dir/server.crt" \
        -subj "/CN=gateway-lb" \
        -addext "subjectAltName=DNS:*,DNS:localhost,IP:127.0.0.1"
    echo "Certificate generated in $cert_dir/"
}

do_start() {
    if [ ! -f "$CONFIG_FILE" ]; then
        echo "Error: config file not found: $CONFIG_FILE"
        exit 1
    fi

    # Check if config has TLS enabled
    if grep -q "^tls:" "$CONFIG_FILE"; then
        generate_cert
    fi

    # Build image if not exists or if source changed
    echo "Building Docker image ${IMAGE_NAME}..."
    docker build -t "$IMAGE_NAME" "$SCRIPT_DIR"

    # Stop existing container
    do_stop

    echo "Starting ${CONTAINER_NAME}..."

    local -a docker_opts=(
        -d
        --name "$CONTAINER_NAME"
        --restart unless-stopped
        -v "$(cd "$(dirname "$CONFIG_FILE")" && pwd)/$(basename "$CONFIG_FILE"):/etc/gateway/routes.yaml:ro"
    )

    # Mount certs if TLS is enabled
    if [ -d "$SCRIPT_DIR/certs" ]; then
        docker_opts+=(
            -v "$SCRIPT_DIR/certs/server.crt:/etc/gateway/server.crt:ro"
            -v "$SCRIPT_DIR/certs/server.key:/etc/gateway/server.key:ro"
        )
    fi

    # Detect ports from config
    local http_port https_port
    http_port=$(grep "^listen_port:" "$CONFIG_FILE" | awk '{print $2}' | tr -d '"'"'")
    https_port=$(grep "port:" "$CONFIG_FILE" | head -2 | tail -1 | awk '{print $2}' | tr -d '"'"'')

    [ -n "$http_port" ] && docker_opts+=(-p "${http_port}:${http_port}")
    [ -n "$https_port" ] && docker_opts+=(-p "${https_port}:${https_port}")

    docker run "${docker_opts[@]}" "$IMAGE_NAME"

    echo ""
    echo "${CONTAINER_NAME} started."
    do_status
}

case "$ACTION" in
    start)   do_start ;;
    stop)    do_stop ;;
    restart) do_stop; do_start ;;
    status)  do_status ;;
    -h|--help|help) usage ;;
    *)       echo "Unknown command: $ACTION"; usage ;;
esac
