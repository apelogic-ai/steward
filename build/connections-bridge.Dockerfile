FROM rust:1.95.0-bookworm@sha256:6258907abe69656e41cd992e0b705cdcfabcbbe3db374f92ed2d47121282d4a1 AS build
WORKDIR /workspace
COPY . .
RUN cargo build --locked --release --bin steward-connections-bridge

FROM busybox:1.37.0-musl@sha256:fc6dddc4c44b1bfe37f41cae8e67d1693828e8f42a91862816d7953e2c9d3f23 AS toolbox
RUN mkdir -p /sandbox \
  && chown 65532:65532 /sandbox \
  && ln -sf busybox /bin/tar \
  && ln -sf busybox /bin/ip \
  && ln -sf busybox /bin/id \
  && ln -sf busybox /bin/mkdir \
  && ln -sf busybox /bin/rm

FROM gcr.io/distroless/cc-debian12:nonroot@sha256:fccdbb0a547c14e23fcf4ce8ad62ca5d43b4faae8d22cd292f490fef9946c96e

COPY --from=toolbox /bin/busybox /bin/busybox
COPY --from=toolbox /bin/sh /bin/sh
COPY --from=toolbox /bin/tar /bin/tar
COPY --from=toolbox /bin/ip /bin/ip
COPY --from=toolbox /bin/id /bin/id
COPY --from=toolbox /bin/mkdir /bin/mkdir
COPY --from=toolbox /bin/rm /bin/rm
COPY --from=build /workspace/target/release/steward-connections-bridge /usr/local/bin/steward-connections-bridge
COPY --chown=65532:65532 --from=toolbox /sandbox /sandbox

USER 65532:65532
ENTRYPOINT ["/usr/local/bin/steward-connections-bridge"]
