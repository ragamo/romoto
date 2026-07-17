FROM alpine:3.20
ARG TARGETARCH
ARG VERSION
RUN apk add --no-cache curl
RUN ARCH=$(case "$TARGETARCH" in "amd64") echo "x86_64-unknown-linux-musl" ;; "arm64") echo "aarch64-unknown-linux-musl" ;; esac) && \
    curl -fsSL "https://github.com/ragamo/romoto/releases/download/v${VERSION}/romoto-${ARCH}.tar.gz" | tar xz -C /usr/local/bin
ENTRYPOINT ["romoto"]
