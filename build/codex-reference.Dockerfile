FROM ghcr.io/nvidia/openshell-community/sandboxes/base@sha256:aeef1c63f00e2913ea002ccb3aaf925f338b5c5d70e63576f0d95c16a138044e

USER root
ARG LINUX_LIBC_DEV_VERSION=6.8.0-142.142
ARG LINUX_LIBC_DEV_SHA256=937db1a88a4fa2ea97fd4eab89f2cd9d077290f6a26bcd27b1a6d24fa3d706b6
ARG RIPGREP_VERSION=14.1.0-1
ARG RIPGREP_SHA256=c5ae63c7bee915b1cfc9f0bd07c55b4ce7f2bcd1133cba4da56719aac26101a4
ARG JQ_VERSION=1.7.1-3ubuntu0.24.04.2
ARG JQ_SHA256=f59183392f84fe67b407b494d8ba6185e9b0e4a106ffcb369490bd20a83fd40e
ARG LIBJQ_VERSION=1.7.1-3ubuntu0.24.04.2
ARG LIBJQ_SHA256=078fbf38b2e7c3f24cc5dc894d1bb03b617894b887a6e13d5943e13818e724f3
ARG LIBONIG_VERSION=6.9.9-1build1
ARG LIBONIG_SHA256=605f42287d2bcb6c84f5a02377c7cd777090c5e857259a4030a571989f19dde2
ARG FD_FIND_VERSION=9.0.0-1
ARG FD_FIND_SHA256=2f4f1bf881e3e967f7ef3da0b757638f4b0db5227f21b2ac0aaa89ae76e6672d
ARG TREE_VERSION=2.1.1-2ubuntu3.24.04.2
ARG TREE_SHA256=56edc302c11b1d35975ded0a38d52e98e82280b12a7a1c7a1b03e4772061731e
RUN curl --fail --silent --show-error --location \
        "https://launchpad.net/ubuntu/+archive/primary/+files/linux-libc-dev_${LINUX_LIBC_DEV_VERSION}_amd64.deb" \
        --output /tmp/linux-libc-dev.deb \
    && curl --fail --silent --show-error --location \
        "https://launchpad.net/ubuntu/+archive/primary/+files/ripgrep_${RIPGREP_VERSION}_amd64.deb" \
        --output /tmp/ripgrep.deb \
    && curl --fail --silent --show-error --location \
        "https://launchpad.net/ubuntu/+archive/primary/+files/jq_${JQ_VERSION}_amd64.deb" \
        --output /tmp/jq.deb \
    && curl --fail --silent --show-error --location \
        "https://launchpad.net/ubuntu/+archive/primary/+files/libjq1_${LIBJQ_VERSION}_amd64.deb" \
        --output /tmp/libjq1.deb \
    && curl --fail --silent --show-error --location \
        "https://launchpad.net/ubuntu/+archive/primary/+files/libonig5_${LIBONIG_VERSION}_amd64.deb" \
        --output /tmp/libonig5.deb \
    && curl --fail --silent --show-error --location \
        "https://launchpad.net/ubuntu/+archive/primary/+files/fd-find_${FD_FIND_VERSION}_amd64.deb" \
        --output /tmp/fd-find.deb \
    && curl --fail --silent --show-error --location \
        "https://launchpad.net/ubuntu/+archive/primary/+files/tree_${TREE_VERSION}_amd64.deb" \
        --output /tmp/tree.deb \
    && printf '%s  %s\n' "${LINUX_LIBC_DEV_SHA256}" /tmp/linux-libc-dev.deb | sha256sum --check --strict \
    && printf '%s  %s\n' "${RIPGREP_SHA256}" /tmp/ripgrep.deb | sha256sum --check --strict \
    && printf '%s  %s\n' "${JQ_SHA256}" /tmp/jq.deb | sha256sum --check --strict \
    && printf '%s  %s\n' "${LIBJQ_SHA256}" /tmp/libjq1.deb | sha256sum --check --strict \
    && printf '%s  %s\n' "${LIBONIG_SHA256}" /tmp/libonig5.deb | sha256sum --check --strict \
    && printf '%s  %s\n' "${FD_FIND_SHA256}" /tmp/fd-find.deb | sha256sum --check --strict \
    && printf '%s  %s\n' "${TREE_SHA256}" /tmp/tree.deb | sha256sum --check --strict \
    && PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin dpkg --install \
        /tmp/linux-libc-dev.deb \
        /tmp/ripgrep.deb \
        /tmp/libonig5.deb \
        /tmp/libjq1.deb \
        /tmp/jq.deb \
        /tmp/fd-find.deb \
        /tmp/tree.deb \
    && rm /tmp/linux-libc-dev.deb /tmp/ripgrep.deb /tmp/libonig5.deb \
        /tmp/libjq1.deb /tmp/jq.deb /tmp/fd-find.deb /tmp/tree.deb \
    && ln -s /usr/bin/fdfind /usr/local/bin/fd \
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
    && test "$(node --print 'require("/usr/lib/node_modules/tar/package.json").version')" = "7.5.22" \
    && test "$(node --print 'require("/usr/lib/node_modules/npm/node_modules/tar/package.json").version')" = "7.5.22" \
    && test -x /usr/lib/node_modules/@openai/codex/node_modules/@openai/codex-linux-x64/vendor/x86_64-unknown-linux-musl/codex/codex

USER sandbox
RUN rg --version \
    && jq --version \
    && fd --version \
    && tree --version
