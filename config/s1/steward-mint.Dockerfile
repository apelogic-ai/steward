# syntax=docker/dockerfile:1.19.0@sha256:b6afd42430b15f2d2a4c5a02b919e98a525b785b1aaff16747d2f623364e39b6
FROM rust:1.95.0-bookworm@sha256:6258907abe69656e41cd992e0b705cdcfabcbbe3db374f92ed2d47121282d4a1 AS build

WORKDIR /src
COPY . .
RUN cargo build --locked --release -p steward-mint-bin

FROM debian:bookworm-slim@sha256:7b140f374b289a7c2befc338f42ebe6441b7ea838a042bbd5acbfca6ec875818
COPY --from=build /src/target/release/steward-mint-bin /usr/local/bin/steward-mint
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/steward-mint"]
