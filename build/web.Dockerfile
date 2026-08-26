FROM oven/bun:1.2.21-slim@sha256:9759e7229cd7c2939d960420bdb8dc5dc3b3dda0285f8601226606e5fd97dfdf AS bun

FROM node:26.5.0-bookworm-slim@sha256:2d49d876e96237d76de412761cf05dbfe5aee325cc4406a4d41d5824c5bb8beb AS build
COPY --from=bun /usr/local/bin/bun /usr/local/bin/bun
ENV NEXT_TELEMETRY_DISABLED=1
WORKDIR /workspace
COPY . .
RUN bun install --frozen-lockfile
RUN bun run --cwd web build

FROM gcr.io/distroless/nodejs24-debian12:nonroot@sha256:14d42e2511532589a7c7e01a753667a74fcc96266e137e8125006b87b0c32d0a
ENV HOSTNAME=0.0.0.0 \
    NEXT_TELEMETRY_DISABLED=1 \
    NODE_ENV=production \
    PORT=3000
WORKDIR /app
COPY --chown=65532:65532 --from=build /workspace/web/.next/standalone ./
COPY --chown=65532:65532 --from=build /workspace/web/.next/static ./web/.next/static
COPY --chown=65532:65532 --from=build /workspace/web/public ./web/public
USER 65532:65532
CMD ["web/server.js"]
