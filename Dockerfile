# Multi-stage production build for Ferrite Control Plane & Ferrous Agent
FROM rust:1.78-slim as builder

WORKDIR /app
COPY . .

RUN cargo build --release --package ferrite-server --package ferrous

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    git \
    ripgrep \
    openssh-client \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --no-create-home --shell /usr/sbin/nologin ferrite

WORKDIR /app
COPY --from=builder /app/target/release/ferrite /app/ferrite
COPY --from=builder /app/target/release/ferrous-agent /app/ferrous-agent
RUN chown -R ferrite:ferrite /app

# ferrite.yaml is not baked into the image: it holds credentials, and an image
# is meant to be shared/pulled while config is meant to be per-deployment.
# Provide it at runtime, e.g. `-v ./ferrite.yaml:/app/ferrite.yaml:ro`. This
# image is a dev/test convenience; for production prefer the systemd path in
# packaging/systemd/ (see docs/SECURITY.md).
USER ferrite

EXPOSE 8080 9090

ENTRYPOINT ["/app/ferrite", "serve"]
