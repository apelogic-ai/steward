FROM ghcr.io/nvidia/openshell-community/sandboxes/base@sha256:aeef1c63f00e2913ea002ccb3aaf925f338b5c5d70e63576f0d95c16a138044e

USER root
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
COPY --chmod=0755 build/claude-code-launcher.sh /usr/local/bin/claude
RUN curl --fail --silent --show-error --location \
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
    && printf '%s  %s\n' "${RIPGREP_SHA256}" /tmp/ripgrep.deb | sha256sum --check --strict \
    && printf '%s  %s\n' "${JQ_SHA256}" /tmp/jq.deb | sha256sum --check --strict \
    && printf '%s  %s\n' "${LIBJQ_SHA256}" /tmp/libjq1.deb | sha256sum --check --strict \
    && printf '%s  %s\n' "${LIBONIG_SHA256}" /tmp/libonig5.deb | sha256sum --check --strict \
    && printf '%s  %s\n' "${FD_FIND_SHA256}" /tmp/fd-find.deb | sha256sum --check --strict \
    && printf '%s  %s\n' "${TREE_SHA256}" /tmp/tree.deb | sha256sum --check --strict \
    && PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin dpkg --install \
        /tmp/ripgrep.deb \
        /tmp/libonig5.deb \
        /tmp/libjq1.deb \
        /tmp/jq.deb \
        /tmp/fd-find.deb \
        /tmp/tree.deb \
    && rm /tmp/ripgrep.deb /tmp/libonig5.deb \
        /tmp/libjq1.deb /tmp/jq.deb /tmp/fd-find.deb /tmp/tree.deb \
    && ln -s /usr/bin/fdfind /usr/local/bin/fd \
    && npm install --global --ignore-scripts=false @anthropic-ai/claude-code@2.1.222 \
    && test -x /usr/bin/claude \
    && PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin apt-get purge --yes --auto-remove linux-libc-dev

USER sandbox
RUN test "$(/usr/bin/claude --version)" = "2.1.222 (Claude Code)" \
    && test ! -e "$HOME/.claude.json" \
    && test ! -e "$HOME/.claude/.credentials.json" \
    && test ! -e "$HOME/.credentials.json" \
    && test ! -e "$HOME/.config/claude" \
    && ! find "$HOME/.claude" -type f -name 'settings*.json' \
        -exec grep -El '"(apiKeyHelper|ANTHROPIC_API_KEY|ANTHROPIC_AUTH_TOKEN|CLAUDE_CODE_OAUTH_TOKEN|AWS_ACCESS_KEY_ID|AWS_SECRET_ACCESS_KEY|AWS_SESSION_TOKEN|AWS_SECURITY_TOKEN)"[[:space:]]*:' {} + \
        | grep -q . \
    && ! grep -ERq 'sk-ant-[A-Za-z0-9_-]{20,}|(AKIA|ASIA)[A-Z0-9]{16}' "$HOME/.claude" \
    && rg --version \
    && jq --version \
    && fd --version \
    && tree --version
