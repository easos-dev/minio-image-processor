# 一次性初始化客户端也从官方固定源码构建；不依赖 latest 标签。
FROM golang:1.26.8-bookworm@sha256:dc9ad6c05acc7a88e5b71bde60a5fe3bd4b9f0db209011711b464107438a8107 AS build
WORKDIR /src
RUN curl -fsSL https://codeload.github.com/minio/mc/tar.gz/d6541ea280b73a834b64d4097e21f2be77676104 \
    | tar xz --strip-components=1
RUN CGO_ENABLED=0 GOMAXPROCS=2 go build -p 2 -trimpath -ldflags='-s -w' -o /out/mc .
FROM debian:bookworm-slim@sha256:7c7b2c966bc9ee8cedfeef67e0e279108992c77681fa595db4a9d65c06ccc587
COPY --from=build /out/mc /usr/local/bin/mc
COPY --from=build /src/LICENSE /usr/share/doc/mc/LICENSE
LABEL org.opencontainers.image.source="https://github.com/minio/mc" \
      org.opencontainers.image.revision="d6541ea280b73a834b64d4097e21f2be77676104" \
      org.opencontainers.image.licenses="AGPL-3.0-only"
ENV MC_CONFIG_DIR=/tmp/mc
ENTRYPOINT ["mc"]
