# syntax=docker/dockerfile:1.19.0@sha256:b6afd42430b15f2d2a4c5a02b919e98a525b785b1aaff16747d2f623364e39b6
FROM oven/bun:1.2.21@sha256:5a2011bf09364b9af658ac1e66f60d08092f4291aeefbff448d58b027734fdd0

ARG MCP_GW_COMMIT
ARG MCP_GW_PATCH_SHA256
LABEL org.opencontainers.image.revision="${MCP_GW_COMMIT}"
LABEL agents.apelogic.ai/patch-sha256="${MCP_GW_PATCH_SHA256}"

WORKDIR /app
COPY package.json bun.lock tsconfig.json ./
COPY src ./src
COPY shared ./shared
COPY servers ./servers
RUN bun install --frozen-lockfile

ENV NODE_ENV=production
ENV PORT=8080
CMD ["bun", "run", "servers/github-mcp/wrapper/src/main.ts"]
