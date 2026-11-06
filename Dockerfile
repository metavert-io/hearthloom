# Hearthloom remote node (deployment modes B/D): the same daemon,
# remote posture — bound 0.0.0.0 behind Fly's TLS edge, locked by
# bootstrap immediately after first deploy. One volume = one brain.
# Base images are pinned by digest (Dependabot proposes updates): a
# floating tag would build whatever it points at that day.
FROM rust:1.99.1-bookworm@sha256:e6f2cff35d0f309acf8433b4f05878ed77092602c5065cc81801177da0bb3a9c AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p hearthloom-cli

FROM debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc
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
