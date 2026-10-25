# Hearthloom remote node (deployment modes B/D): the same daemon,
# remote posture — bound 0.0.0.0 behind Fly's TLS edge, locked by
# bootstrap immediately after first deploy. One volume = one brain.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p hearthloom-cli

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/hearthloom /usr/local/bin/hearthloom
# Optional hosted defaults (scripts/build-tenant-image.sh passes them from
# deploy.env); empty means a plain self-hosted node.
ARG HEARTHLOOM_REGISTRY_URL=""
ARG HEARTHLOOM_PROVISIONER_URL=""
# The brain lives on the mounted volume — nothing precious in the image.
ENV HEARTHLOOM_HOME=/data/.hearthloom \
    HEARTHLOOM_HOST=0.0.0.0 \
    HEARTHLOOM_PORT=43278 \
    HEARTHLOOM_REGISTRY_URL=${HEARTHLOOM_REGISTRY_URL} \
    HEARTHLOOM_PROVISIONER_URL=${HEARTHLOOM_PROVISIONER_URL}
EXPOSE 43278
CMD ["hearthloom", "daemon", "run"]
