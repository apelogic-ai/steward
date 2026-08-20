FROM rust:1.95.0-bookworm@sha256:6258907abe69656e41cd992e0b705cdcfabcbbe3db374f92ed2d47121282d4a1 AS build
WORKDIR /workspace
COPY . .
RUN cargo build --locked --release --bin steward-connections-bridge

FROM gcr.io/distroless/cc-debian12:nonroot@sha256:fccdbb0a547c14e23fcf4ce8ad62ca5d43b4faae8d22cd292f490fef9946c96e
COPY --from=build /workspace/target/release/steward-connections-bridge /usr/local/bin/steward-connections-bridge
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/steward-connections-bridge"]
