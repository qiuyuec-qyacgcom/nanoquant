# 全量 Rust 迁移后的唯一 Dockerfile（Rust 构建阶段 + Debian slim 运行阶段）。
# 构建：docker build -t nanoquant:latest .
# 运行阶段不包含解释器运行时、包管理器与解释器健康检查。

# ---- 前端构建 ----
FROM node:20.18-alpine AS frontend
WORKDIR /build
COPY frontend/package.json frontend/package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY frontend/ ./
RUN npm run build

# ---- Rust 构建 ----
# 依赖最大 MSRV 为 1.88（icu_* 2.4.0 等使用 edition 2024），1.83 无法编译本锁文件。
FROM rust:1.90-slim AS backend
WORKDIR /build
# rusqlite/libsqlite3-sys (bundled) and ring need a C toolchain; rust:slim does not ship one.
RUN apt-get update \
    && apt-get install -y --no-install-recommends gcc libc6-dev make perl \
    && rm -rf /var/lib/apt/lists/*
COPY rust/nanoquant/Cargo.toml rust/nanoquant/Cargo.lock ./
COPY rust/nanoquant/src ./src
COPY rust/exec-core/Cargo.toml rust/exec-core/Cargo.lock ./exec-core/
COPY rust/exec-core/src ./exec-core/src
RUN cargo build --release --locked \
    && cargo build --release --locked --manifest-path exec-core/Cargo.toml

# ---- 运行时 ----
FROM debian:bookworm-slim
ENV NANOQUANT_HOME=/app \
    NANOQUANT_PORT=8899 \
    NANOQUANT_DB=/app/data/nanoquant.db \
    NANOQUANT_FRONTEND=/app/frontend/dist
WORKDIR /app

RUN useradd --create-home --uid 1000 nanoquant

COPY --from=backend /build/target/release/nanoquant /usr/local/bin/nanoquant
COPY --from=backend /build/exec-core/target/release/exec-core /usr/local/bin/exec-core
COPY --from=frontend /build/dist /app/frontend/dist

RUN mkdir -p /app/data /app/logs && chown -R nanoquant:nanoquant /app
USER nanoquant

EXPOSE 8899
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s \
  CMD ["/usr/local/bin/nanoquant", "--healthcheck"]
CMD ["nanoquant"]
