FROM rust:1.85-bookworm AS builder

RUN apt-get update && \
    apt-get install -y --no-install-recommends cmake && \
    rm -rf /var/lib/apt/lists/*

# Use rsproxy.cn mirror for crates.io
RUN mkdir -p /usr/local/cargo && \
    printf '[source.crates-io]\nreplace-with = "rsproxy-sparse"\n\n[source.rsproxy-sparse]\nregistry = "sparse+https://rsproxy.cn/index/"\n' > /usr/local/cargo/config.toml

WORKDIR /app
COPY Cargo.toml ./
COPY src/ ./src/

# Cache dependencies with a dummy build, then build the real binary
RUN mkdir -p src && \
    echo 'fn main() {}' > src/main.rs && \
    cargo build --release && \
    rm -rf src

COPY src/ ./src/
RUN touch src/main.rs && cargo build --release

FROM openeuler/openeuler:24.03

RUN dnf install -y ca-certificates openssl-libs && \
    dnf clean all

COPY --from=builder /app/target/release/gateway-lb /usr/bin/gateway-lb

COPY config.yaml /etc/gateway/routes.yaml

ENV CONFIG_PATH=/etc/gateway/routes.yaml

EXPOSE 6198 6443

CMD ["gateway-lb"]
