FROM rust:1.95.0-bookworm@sha256:6258907abe69656e41cd992e0b705cdcfabcbbe3db374f92ed2d47121282d4a1 AS build
WORKDIR /workspace
COPY . .
RUN cargo build --locked --release \
    --bin steward-apiserver-bin \
    --bin steward-controller-bin \
    --bin steward-mint-bin

FROM debian:bookworm-slim@sha256:7b140f374b289a7c2befc338f42ebe6441b7ea838a042bbd5acbfca6ec875818
RUN apt-get update && apt-get install --yes --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
ARG BINARY
COPY --from=build "/workspace/target/release/${BINARY}" /usr/local/bin/steward
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/steward"]
