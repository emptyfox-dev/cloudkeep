# syntax=docker/dockerfile:1

# ============================================================
# 1. 构建阶段
# ============================================================
# 用 alpine + musl：产出 musl 静态链接的单一二进制，换任何 Linux 都能跑。
# buildx 多架构构建时，每个阶段默认就按目标平台构建（等价于 $TARGETPLATFORM），
# 因此无需显式指定 --platform，也不需要任何交叉编译工具链。
FROM rust:1-alpine AS builder

# ring 需要 C 编译器（build-base 提供 gcc/musl-dev/make），perl 作为兜底
RUN apk add --no-cache build-base perl

# 可选：国内构建时用 crates.io 镜像加速
#   docker build --build-arg CARGO_MIRROR=sparse+https://rsproxy.cn/index/ .
ARG CARGO_MIRROR=""

# 内置到二进制里的服务端地址（构建期注入，见 build.rs）。
# 分发镜像/二进制前请把它设成可公开访问的地址，否则使用者首次运行连不上。
ARG CLOUDKEEP_DEFAULT_SERVER_URL="http://127.0.0.1:49494"

WORKDIR /src

# 先只拷清单，利用层缓存把依赖编译结果固化下来
# build.rs 必须一并拷入：Cargo.toml 声明了 build = "build.rs"，
# 缺少它 cargo 会直接报错（"could not find build.rs"）。
COPY Cargo.toml Cargo.lock build.rs ./

# 用空壳源码预编译依赖，这样后续改业务代码时不必重编依赖
RUN mkdir -p src \
    && printf 'fn main() {}\n' > src/main.rs \
    && printf '\n' > src/lib.rs

RUN if [ -n "$CARGO_MIRROR" ]; then \
      mkdir -p "$CARGO_HOME" \
      && printf '[source.crates-io]\nreplace-with = "mirror"\n\n[source.mirror]\nregistry = "%s"\n' "$CARGO_MIRROR" > "$CARGO_HOME/config.toml" \
      && cat "$CARGO_HOME/config.toml"; \
    fi \
    && cargo build --release --locked \
    && rm -rf src

# 再拷真实源码（touch 一下，避免时间戳导致 cargo 误判为最新）
COPY src ./src
RUN touch src/main.rs src/lib.rs \
    && cargo build --release --locked \
    && ./target/release/cloudkeep --version

# ============================================================
# 2. 导出阶段：只取出静态二进制，用于分发「单文件直接运行」
#    必须放在 runtime 之前，否则它会成为 docker build 的默认目标
#    docker buildx build --target binary --output type=local,dest=dist .
# ============================================================
FROM scratch AS binary
COPY --from=builder /src/target/release/cloudkeep /cloudkeep

# ============================================================
# 3. 运行阶段（默认目标，放在最后）
# ============================================================
FROM alpine:3.21 AS runtime

# ca-certificates：rustls 使用系统信任库，必须安装，否则 HTTPS 握手失败
# 以非 root 的 uid/gid 1000 运行
RUN apk add --no-cache ca-certificates \
    && (addgroup -g 1000 cloudkeep 2>/dev/null || true) \
    && (adduser -D -u 1000 -G cloudkeep -h /home/cloudkeep cloudkeep 2>/dev/null || true) \
    && mkdir -p /tmp/cloudkeep /home/cloudkeep/.cloudkeep \
    && chown -R 1000:1000 /tmp/cloudkeep /home/cloudkeep

COPY --from=builder /src/target/release/cloudkeep /usr/local/bin/cloudkeep

# 子命令符号链接：docker exec 不经过 ENTRYPOINT，
# 因此 `docker exec cloudkeep backup` 需要 PATH 里存在 backup 这个命令。
# 二进制自身支持 busybox 风格的 argv[0] 派发，符号链接即可正常工作。
RUN for c in run backup ensure-dir auth auth-url auth-code healthcheck help; do \
      ln -sf cloudkeep "/usr/local/bin/$c"; \
    done

ENV HOME=/home/cloudkeep \
    TZ=Asia/Shanghai \
    WORK_DIR=/tmp/cloudkeep

USER cloudkeep
WORKDIR /home/cloudkeep

# 心跳文件超过 HEALTHCHECK_MAX_AGE_HOURS（默认 26 小时）未刷新即 unhealthy
HEALTHCHECK --interval=5m --timeout=10s --start-period=2m --retries=3 \
  CMD ["cloudkeep", "healthcheck"]

ENTRYPOINT ["cloudkeep"]
# 默认启动调度；compose run / exec 传入的子命令会替换 CMD
CMD ["run"]
