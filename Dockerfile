FROM debian:bookworm-slim
ARG TARGETARCH
ARG VERSION
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl && rm -rf /var/lib/apt/lists/*
RUN ARCH=$(case "$TARGETARCH" in "amd64") echo "x86_64-unknown-linux-gnu" ;; "arm64") echo "aarch64-unknown-linux-gnu" ;; esac) && \
    curl -fsSL "https://github.com/ragamo/romoto/releases/download/v${VERSION}/romoto-${ARCH}.tar.gz" | tar xz -C /usr/local/bin
ENTRYPOINT ["romoto"]
