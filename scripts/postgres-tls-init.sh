#!/usr/bin/env sh
set -eu

cp /tls-input/server.key "${PGDATA}/server.key"
cp /tls-input/server.crt "${PGDATA}/server.crt"
chmod 600 "${PGDATA}/server.key"

cat >>"${PGDATA}/postgresql.conf" <<'EOF'
ssl = on
ssl_cert_file = 'server.crt'
ssl_key_file = 'server.key'
EOF

cat >"${PGDATA}/pg_hba.conf" <<'EOF'
local all all trust
hostssl all all 0.0.0.0/0 trust
hostssl all all ::/0 trust
hostnossl all all 0.0.0.0/0 reject
hostnossl all all ::/0 reject
EOF
