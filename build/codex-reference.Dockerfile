FROM ghcr.io/nvidia/openshell-community/sandboxes/base@sha256:aeef1c63f00e2913ea002ccb3aaf925f338b5c5d70e63576f0d95c16a138044e

USER root
ARG LINUX_LIBC_DEV_VERSION=6.8.0-142.142
ARG LINUX_LIBC_DEV_SHA256=937db1a88a4fa2ea97fd4eab89f2cd9d077290f6a26bcd27b1a6d24fa3d706b6
RUN curl --fail --silent --show-error --location \
        "https://archive.ubuntu.com/ubuntu/pool/main/l/linux/linux-libc-dev_${LINUX_LIBC_DEV_VERSION}_amd64.deb" \
        --output /tmp/linux-libc-dev.deb \
    && printf '%s  %s\n' "${LINUX_LIBC_DEV_SHA256}" /tmp/linux-libc-dev.deb | sha256sum --check --strict \
    && dpkg --install /tmp/linux-libc-dev.deb \
    && rm /tmp/linux-libc-dev.deb \
    && npm install --global --ignore-scripts=false @openai/codex@0.140.0 tar@7.5.22 \
    && npm pack --silent tar@7.5.22 --pack-destination /tmp \
    && rm -rf /usr/lib/node_modules/npm/node_modules/tar \
    && mkdir -p /usr/lib/node_modules/npm/node_modules/tar \
    && tar --extract --gzip --file /tmp/tar-7.5.22.tgz --strip-components=1 --directory /usr/lib/node_modules/npm/node_modules/tar \
    && rm /tmp/tar-7.5.22.tgz \
    && native_root=/usr/lib/node_modules/@openai/codex/node_modules/@openai/codex-linux-x64/vendor/x86_64-unknown-linux-musl \
    && mkdir -p "${native_root}/codex" \
    && mv "${native_root}/bin/codex" "${native_root}/codex/codex" \
    && ln -s ../codex/codex "${native_root}/bin/codex" \
    && test "$(/usr/bin/codex --version)" = "codex-cli 0.140.0" \
    && test "$(node --print \"require('/usr/lib/node_modules/tar/package.json').version\")" = "7.5.22" \
    && test "$(node --print \"require('/usr/lib/node_modules/npm/node_modules/tar/package.json').version\")" = "7.5.22" \
    && test -x /usr/lib/node_modules/@openai/codex/node_modules/@openai/codex-linux-x64/vendor/x86_64-unknown-linux-musl/codex/codex

USER sandbox
