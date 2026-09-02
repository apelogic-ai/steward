FROM rust:1.95.0-bookworm@sha256:6258907abe69656e41cd992e0b705cdcfabcbbe3db374f92ed2d47121282d4a1 AS build
WORKDIR /workspace
COPY . .
RUN cargo build --locked --release --bin steward-connections-bridge

FROM debian:bookworm-slim@sha256:7b140f374b289a7c2befc338f42ebe6441b7ea838a042bbd5acbfca6ec875818

RUN apt-get update \
  && apt-get install --yes --no-install-recommends iproute2 \
  && rm -rf /var/lib/apt/lists/* \
  && test -x /bin/cat \
  && test -x /bin/find \
  && test -x /bin/id \
  && test -x /bin/ip \
  && test -x /bin/mkdir \
  && test -x /bin/mktemp \
  && test -x /bin/rm \
  && test -x /bin/sh \
  && test -x /bin/sleep \
  && test -x /bin/tar \
  && test -x /bin/touch \
  && mkdir -p /sandbox \
  && chown 65532:65532 /sandbox

COPY --from=build /workspace/target/release/steward-connections-bridge /usr/local/bin/steward-connections-bridge

USER 65532:65532
ENTRYPOINT ["/usr/local/bin/steward-connections-bridge"]
