# 仅在目标服务器构建。Cargo.lock 固定 Rust 依赖，基础镜像使用不可变摘要。
FROM rust:1.97.1-bookworm@sha256:0e2bcaef56d041a486784e54104a81aebe0da44bd03019bd70bc0401e42e4a97 AS build
RUN apt-get update && apt-get install -y --no-install-recommends libvips-dev libmagickwand-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY native ./native
RUN cc -O2 -Wall -Wextra -Werror native/worker.c -o /usr/local/bin/image-worker $(pkg-config --cflags --libs vips MagickWand libheif) -lm
RUN CARGO_BUILD_JOBS=2 cargo build --locked --release

FROM debian:bookworm-slim@sha256:7c7b2c966bc9ee8cedfeef67e0e279108992c77681fa595db4a9d65c06ccc587
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libvips42 libmagickwand-6.q16-6 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /build/target/release/image-processor /usr/local/bin/image-processor
# 使用 Debian 已有 nobody 身份，不创建宿主机或镜像用户。
COPY --from=build /usr/local/bin/image-worker /usr/local/bin/image-worker
USER 65534:65534
ENV IMAGE_PROCESSOR_CONFIG=/etc/image-processor/config.toml
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/image-processor"]
