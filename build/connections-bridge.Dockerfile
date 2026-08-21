FROM rust:1.95.0-bookworm@sha256:6258907abe69656e41cd992e0b705cdcfabcbbe3db374f92ed2d47121282d4a1 AS build
WORKDIR /workspace
COPY . .
RUN cargo build --locked --release --bin steward-connections-bridge

FROM busybox:1.37.0-musl@sha256:fc6dddc4c44b1bfe37f41cae8e67d1693828e8f42a91862816d7953e2c9d3f23 AS toolbox
COPY --chmod=0755 build/connections-bridge-bash /rootfs/usr/bin/bash
RUN mkdir -p /sandbox \
  && mkdir -p /rootfs/usr/bin /rootfs/bin \
  && chown 65532:65532 /sandbox \
  && cp /bin/busybox /rootfs/usr/bin/busybox \
  && for applet in cp find id ip mkdir mktemp rm sh sleep tar touch; do \
    ln -s /usr/bin/busybox /rootfs/usr/bin/"${applet}"; \
    ln -s "/usr/bin/${applet}" "/rootfs/bin/${applet}"; \
  done \
  && ln -s /usr/bin/busybox /rootfs/bin/busybox \
  && ln -s /usr/bin/bash /rootfs/bin/bash

FROM gcr.io/distroless/cc-debian12:nonroot@sha256:fccdbb0a547c14e23fcf4ce8ad62ca5d43b4faae8d22cd292f490fef9946c96e

COPY --from=toolbox /rootfs/usr/bin/ /usr/bin/
COPY --from=toolbox /rootfs/bin/ /bin/
COPY --from=build /workspace/target/release/steward-connections-bridge /usr/local/bin/steward-connections-bridge
COPY --chown=65532:65532 --from=toolbox /sandbox /sandbox

USER 65532:65532
ENTRYPOINT ["/usr/local/bin/steward-connections-bridge"]
