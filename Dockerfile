# syntax=docker/dockerfile:1.7
# 多阶段构建：cargo-chef 缓存依赖层 → musl 静态链接 release → distroless 运行层。
# 基础镜像为 alpine，宿主三元组即 x86_64-unknown-linux-musl，无需额外添加 target。

FROM rust:1.85-alpine AS chef
RUN apk add --no-cache musl-dev
RUN cargo install cargo-chef --locked
WORKDIR /app

# 依赖配方：仅取依赖图，生成可被缓存的构建层
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# 依赖编译层（源码变动时命中缓存）
FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN cargo build --release --locked

# 运行层：仅含静态二进制
FROM gcr.io/distroless/cc-debian12 AS runtime
COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/wei-class /usr/local/bin/wei-class
EXPOSE 8080
ENV LISTEN_ADDR=0.0.0.0:8080
ENV RUST_LOG=info
ENTRYPOINT ["/usr/local/bin/wei-class"]
