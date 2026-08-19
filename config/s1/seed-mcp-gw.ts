import { Pool } from "pg";

import { encryptSecret } from "/app/shared/oauth/crypto";
import {
  OAUTH_SCHEMA_SQL,
  SqlOAuthTokenStore,
} from "/app/shared/oauth/sql-store";

const databaseUrl = process.env.TOKEN_STORE_DSN;
const encryptionKey = process.env.GITHUB_TOKEN_ENCRYPTION_KEY;
const issuer = process.env.HOP1_ISSUER;
const taskFixtureIdentitySubject = process.env.TASK_FIXTURE_IDENTITY_SUBJECT;
if (!databaseUrl || !encryptionKey || !issuer) {
  throw new Error("seed configuration is incomplete");
}

const pool = new Pool({ connectionString: databaseUrl });
await pool.query(OAUTH_SCHEMA_SQL);
const store = new SqlOAuthTokenStore({
  query: (sql, params) => pool.query(sql, params),
});

const taskFixtureHop1Subject = async (): Promise<string | undefined> => {
  if (!taskFixtureIdentitySubject) {
    return undefined;
  }
  const deadline = Date.now() + 60_000;
  while (Date.now() < deadline) {
    const result = await pool
      .query<{ user_id: string }>(
        "SELECT user_id FROM canonical_identity_subjects WHERE issuer = $1 AND subject = $2",
        ["https://accounts.google.com", taskFixtureIdentitySubject],
      )
      .catch(() => undefined);
    const userId = result?.rows[0]?.user_id;
    if (userId) {
      return userId;
    }
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
  throw new Error(
    `task fixture canonical identity was not registered: ${taskFixtureIdentitySubject}`,
  );
};

const aliceHop1Subject =
  (await taskFixtureHop1Subject()) ??
  "usr_0123456789abcdef0123456789abcdef";
const now = new Date();
await store.saveAccount({
  provider: "github",
  hop1Issuer: issuer,
  // User HOP-1 subjects are canonical IDs; the verified email stays separate
  // so an email rename cannot orphan the provider connection.
  hop1Subject: aliceHop1Subject,
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
