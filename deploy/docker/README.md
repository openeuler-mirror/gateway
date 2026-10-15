# boom-gateway 容器镜像（aarch64 / openEuler 底座）

gateway 独立容器：纯 CPU、不需要 NPU 设备；基镜像与 vLLM 容器统一
（`quay.io/ascend/vllm-ascend:v0.27.1rc1-openeuler`，arm64）。镜像**不打包
config**——entrypoint 按环境变量生成，或挂载正式配置；二进制自包含（Dashboard
前端已编译进去），运行期只依赖基镜像 glibc。

## 一、得到镜像

### 场景 A：网络好（quay.io / crates.io 直连可达）

```bash
IMAGE_TAG=boom-gateway:v0.1.0 ./deploy/docker/build.sh
# IMAGE_TAG 不指定时默认用 git 短 SHA 作 tag
```

### 场景 B：国内网络 / 出不了外网（推荐，实测最稳）

quay.io 直连偶发 manifest 超时、rustup/crates 下载失败、或构建机整体出不了
外网（`Failed to connect ... Timeout was reached`）时，基镜像走 NJU、工具链
和 crates 全走 rsproxy：

```bash
# 1. 基镜像：daemon 拉取（按层断点续传，失败重跑即续传）+ 本地 tag
docker pull quay.nju.edu.cn/ascend/vllm-ascend:v0.27.1rc1-openeuler
docker tag quay.nju.edu.cn/ascend/vllm-ascend:v0.27.1rc1-openeuler boom-base:v0.27.1rc1-openeuler

# 2. 构建（基镜像走本地引用，构建零 registry 依赖）。出不了外网时
#    RUSTUP_INSTALLER 也必须换——sh.rustup.rs 本身连不上，只换后三个没用；
#    二进制直链最自包含。脚本形态（rsproxy.cn/rustup-init.sh）Dockerfile 也收，
#    但该脚本会再从 RUSTUP_UPDATE_ROOT 下载二进制，此变量必须同指镜像
BASE_IMAGE=boom-base:v0.27.1rc1-openeuler \
RUSTUP_INSTALLER=https://rsproxy.cn/rustup/dist/aarch64-unknown-linux-gnu/rustup-init \
RUSTUP_DIST_SERVER=https://rsproxy.cn \
RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup \
CRATES_MIRROR=https://rsproxy.cn/index/ \
  ./deploy/docker/build.sh
```

### 场景 C：目标机不构建——导出压缩包拷过去

```bash
# 有镜像的机器上导出（~8.5GB；层本身已是 gzip，再压收益极小）
docker save boom-gateway:v0.1.0-arm64 -o boom-gateway-v0.1.0-arm64.tar
docker save boom-gateway:v0.1.0-arm64 | gzip > boom-gateway-v0.1.0-arm64.tar.gz   # 需要单文件时

scp boom-gateway-v0.1.0-arm64.tar user@target:

# 目标机导入（.tar.gz 也可直接 -i，自动识别压缩）
docker load -i boom-gateway-v0.1.0-arm64.tar
```

### 场景 D：公司代理 + TLS MITM（交付机实测配方）

公司网络须走代理、且代理是 TLS MITM 时（curl 报 `self-signed certificate in
certificate chain`）：代理变量 export 过即自动透传，再带 `PROXY_CA`——宿主机
能走代理，公司 CA 就已在其 bundle 内：

```bash
export https_proxy=http://<代理地址> http_proxy=http://<代理地址>
PROXY_CA=/etc/pki/tls/certs/ca-bundle.crt \
BASE_IMAGE=boom-base:<本地tag> \
RUSTUP_INSTALLER=https://rsproxy.cn/rustup/dist/aarch64-unknown-linux-gnu/rustup-init \
RUSTUP_DIST_SERVER=https://rsproxy.cn \
RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup \
CRATES_MIRROR=https://rsproxy.cn/index/ \
  ./deploy/docker/build.sh
```

镜像源变量按需增减（见场景 B）；`PROXY_CA` 也收目录（如
`/etc/pki/ca-trust/source/anchors/`，自动拼接其中全部 .pem/.crt）。

构建前验 CA（可选。经代理别用 `-sI | head`——那只显示 CONNECT 行，会掩盖
TLS 失败）：`200` = bundle 可用；`000` = bundle 没有公司 CA，从代理现抽：

```bash
docker run --rm -e https_proxy="$https_proxy" -e http_proxy="$http_proxy" \
  -v /etc/pki/tls/certs/ca-bundle.crt:/etc/pki/tls/certs/ca-bundle.crt:ro \
  --entrypoint curl "$BASE_IMAGE" -s -o /dev/null -w '%{http_code}\n' --max-time 10 https://rsproxy.cn
# 抽链后改用 PROXY_CA=/tmp/mitm.pem 构建：
# openssl s_client -proxy <host:port> -connect rsproxy.cn:443 -showcerts </dev/null 2>/dev/null \
#   | awk '/BEGIN CERTIFICATE/,/END CERTIFICATE/' > /tmp/mitm.pem
```

构建后冒烟（完整参数见「二、运行」）：

```bash
docker run -d --name boom-gateway -p 4000:4000 \
  -e MASTER_KEY=sk-xxx -e UPSTREAM_BASE_URL=http://<vllm地址>:8000/v1 \
  --restart unless-stopped boom-gateway:<tag>
curl http://127.0.0.1:4000/health/live    # 期望 200
```

### 附加 cargo feature（BUILD_FEATURES）

默认只带 release 默认 features（otlp）。需要试验性能力时用 `BUILD_FEATURES`
追加（与其他环境变量同排法，多个用空格分隔）：

```bash
# debug-tools：Dashboard 可选 L2 kvc_aware 调度策略 + Debug 页
# （release 默认隐藏，见 app.js buildPolicyOptions / handlers_static 注入
#   window.__KVC_DEBUG）
BUILD_FEATURES=debug-tools ./deploy/docker/build.sh
```

注意 `debug-tools` 同时会放开 Dashboard 的 Debug 页面，**不建议用于交付镜像**；
后端对 `kvc_aware` 策略无 feature 门槛，YAML 直配 `schedule_policy: kvc_aware`
同样生效（UI 中显示为禁用但选中）。

### 换基镜像版本

`BASE_IMAGE` 重新赋值即可，两个 stage 一起换（glibc 一致性由设计保证）。
三个注意点：

- 换 tag 前确认 registry 上存在（NJU 按需回源，未实测的 tag 不保证）
- 换 base 时**不要** `KEEP_CACHE=1`：cargo target 缓存不感知底座变化，
  降级/换发行版可能链接出旧 glibc 符号产物，运行时才爆 `GLIBC` 错误
  （默认的自动清缓存天然规避此坑）
- 构建成功后旧 `boom-gateway:*` 会被清理——留旧版本回滚先
  `docker save`，或 `docker tag` 到非 `boom-gateway:` 前缀（清理只匹配该前缀）

```bash
# 先冒烟再出正式镜像（build.sh 成功后清缓存，反过来的话 verify 要全量重建）
export BASE_IMAGE=boom-base:v0.27.1rc1-a3-openeuler \
       RUSTUP_DIST_SERVER=https://rsproxy.cn \
       RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup \
       CRATES_MIRROR=https://rsproxy.cn/index/
docker pull quay.nju.edu.cn/ascend/vllm-ascend:v0.27.1rc1-a3-openeuler
docker tag quay.nju.edu.cn/ascend/vllm-ascend:v0.27.1rc1-a3-openeuler "$BASE_IMAGE"
./deploy/docker/verify.sh     # 冒烟（通过后删冒烟镜像，缓存保留）
./deploy/docker/build.sh      # 正式镜像（复用上面的缓存，秒级）
```
- 编译全部发生在 builder 容器内，宿主机不需要 rust

## 二、运行

### 方式一：纯环境变量（零挂载，最快起）

```bash
docker run -d --name boom-gateway -p 4000:4000 \
  -e MASTER_KEY=sk-xxx \
  -e UPSTREAM_BASE_URL=http://<vllm地址>:8000/v1 \
  -e UPSTREAM_MODEL=<模型名> \
  -e UPSTREAM_API_KEY=sk-dummy \
  --restart unless-stopped \
  boom-gateway:v0.1.0-arm64
```

| 变量 | 必填 | 默认 | 说明 |
|---|---|---|---|
| `MASTER_KEY` | 是* | - | 主密钥（*仅无挂载 config 时必填） |
| `UPSTREAM_BASE_URL` | 是* | - | 上游 api_base（vLLM 地址） |
| `UPSTREAM_MODEL` | 否 | `default` | 上游模型名 |
| `UPSTREAM_API_KEY` | 否 | `sk-dummy` | 上游 api_key |
| `GATEWAY_PORT` | 否 | `4000` | 监听端口（同时影响 HEALTHCHECK） |
| `GATEWAY_WORKERS` | 否 | `8` | worker 线程数 |
| `CONFIG_PATH` | 否 | `/app/config/config.yaml` | config 路径 |
| `RUST_LOG` | 否 | `info` | 日志级别 |

网页 UI：`http://<宿主机IP>:4000/dashboard`（`/` 自动跳转）。

### 方式二：挂载正式配置

```bash
docker run -d --name boom-gateway -p 4000:4000 \
  -v /etc/boom/config.yaml:/app/config/config.yaml \
  --restart unless-stopped \
  boom-gateway:v0.1.0-arm64
```

最小可用 config（单 vLLM 上游；`"*"` 是兜底路由——请求的模型名匹配不到任何
配置时打到这条部署，不是 litellm 的通配符）：

```yaml
# /etc/boom/config.yaml
model_list:
  - model_name: "*"
    litellm_params:
      model: openai/default        # default 需与 vLLM 的 --served-model-name 一致
      api_key: sk-dummy
      api_base: http://192.168.1.100:8000/v1   # ← 换成真实 vLLM 地址
general_settings:
  master_key: sk-test-master       # ← 换成你的主密钥
server:
  host: 0.0.0.0
  port: 4000
  workers: 32
```

挂载的 config 原样使用；YAML 原生支持 `${VAR}` / `os.environ/VAR` 环境变量
展开。完整字段（多部署负载均衡、限流、计费、DB 等）见仓库根 `config.example.yaml`。

### 改配置不重启

```bash
vi /path/to/config.yaml           # 改挂载的 config
docker kill -s HUP boom-gateway   # 热加载（不清限流计数）
```

容器可写层里手改的 config（`docker exec` 进去改）在 `docker rm` 后丢失——
临时调试随便改，正式部署走挂载。

## 三、运维速查

```bash
docker logs -f boom-gateway               # JSON 日志（stdout）
docker kill -s HUP boom-gateway           # 热加载配置
docker stop boom-gateway                  # 优雅退出（日志出现 "Shutdown complete"）
curl http://localhost:4000/health/live    # 存活探针（编排层/负载均衡用）
curl http://localhost:4000/health/ready   # 就绪探针（配了 DB 且未连上时 503）
```

镜像内置 HEALTHCHECK（`/health/live`，10s 间隔、30s 宽容期），配
`--restart unless-stopped` 或 k8s liveness probe 使用。

## 四、提交前冒烟

```bash
./deploy/docker/verify.sh          # 构建 + health/chat 探针 + 优雅退出断言，自动清容器和冒烟镜像
./deploy/docker/verify.sh --clean  # 同上，并额外清理 buildkit 缓存
```

## 附录

### 文件清单

| 文件 | 用途 |
|---|---|
| `Dockerfile` | 多阶段构建：builder/runtime 同基镜像（glibc 零风险） |
| `entrypoint.sh` | 无 config 时环境变量生成最小配置；exec 使 gateway 为 PID 1 |
| `build.sh` | 一键构建（含中间产物清理） |
| `verify.sh` | 本地冒烟 |
| `proxy-ca.pem` | PROXY_CA 注入点（仓库里是空占位；build.sh 构建期写入、退出还原，不提交内容） |
| `config/config.example.yaml` | 参考示例（不进镜像） |

### 镜像源实测（2026-10，Mac 家宽；各源内容 digest 与官方一致）

| 源 | 可用性 | 实测速度 | 备注 |
|---|---|---|---|
| `quay.io`（官方默认） | ✓ | 好时 ~2MB/s，差时 KB/s | 偶发 manifest HEAD 超时；build.sh 已带 5 次重试 |
| `quay.nju.edu.cn`（南大） | ✓ | 多流聚合 ~2.3MB/s | daemon pull 按层续传；教育网内更快；场景 B 首选 |
| `quay.m.daocloud.io`（DaoCloud） | ✓ | ~0.4MB/s | 免费公共源限速，兜底 |
| `swr.cn-north-4.myhuaweicloud.com/ascendhub` | ✗ | - | 匿名拒绝，需华为云凭证 |

rust 工具链：`rsproxy.cn` 全套可用（含 1.95.0）；USTC
（`https://mirrors.ustc.edu.cn/rust-static`）备选；**TUNA 缺 1.95.0，不可用**。
