import { Pool } from "pg";

import { encryptSecret } from "/app/shared/oauth/crypto";
import {
  OAUTH_SCHEMA_SQL,
  SqlOAuthTokenStore,
} from "/app/shared/oauth/sql-store";

const databaseUrl = process.env.TOKEN_STORE_DSN;
const encryptionKey = process.env.GITHUB_TOKEN_ENCRYPTION_KEY;
const issuer = process.env.HOP1_ISSUER;
if (!databaseUrl || !encryptionKey || !issuer) {
  throw new Error("seed configuration is incomplete");
}

const pool = new Pool({ connectionString: databaseUrl });
await pool.query(OAUTH_SCHEMA_SQL);
const store = new SqlOAuthTokenStore({
  query: (sql, params) => pool.query(sql, params),
});
const now = new Date();
await store.saveAccount({
  provider: "github",
  hop1Issuer: issuer,
  // User HOP-1 subjects are canonical IDs; the verified email stays separate
  // so an email rename cannot orphan the provider connection.
  hop1Subject: "usr_0123456789abcdef0123456789abcdef",
  email: "alice@example.com",
  scopesGranted: ["repo"],
  encryptedRefreshToken: encryptSecret("fixture-provider-token", encryptionKey),
  createdAt: now,
  updatedAt: now,
});
await store.saveAccount({
  provider: "github",
  hop1Issuer: issuer,
  hop1Subject: "service:scheduled-scanner",
  email: "service:scheduled-scanner",
  scopesGranted: ["repo"],
  encryptedRefreshToken: encryptSecret(
    "fixture-service-provider-token",
    encryptionKey,
  ),
  createdAt: now,
  updatedAt: now,
});
await pool.end();
