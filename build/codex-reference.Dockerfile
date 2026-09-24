FROM ghcr.io/nvidia/openshell-community/sandboxes/base@sha256:aeef1c63f00e2913ea002ccb3aaf925f338b5c5d70e63576f0d95c16a138044e

USER root
RUN npm install --global --ignore-scripts=false @openai/codex@0.140.0 \
    && native_root=/usr/lib/node_modules/@openai/codex/node_modules/@openai/codex-linux-x64/vendor/x86_64-unknown-linux-musl \
    && mkdir -p "${native_root}/codex" \
    && mv "${native_root}/bin/codex" "${native_root}/codex/codex" \
    && ln -s ../codex/codex "${native_root}/bin/codex" \
    && test "$(/usr/bin/codex --version)" = "codex-cli 0.140.0" \
    && test -x /usr/lib/node_modules/@openai/codex/node_modules/@openai/codex-linux-x64/vendor/x86_64-unknown-linux-musl/codex/codex

USER sandbox
