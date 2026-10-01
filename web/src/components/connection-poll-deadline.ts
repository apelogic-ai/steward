const MAX_CONNECTION_POLL_MS = 60_000;
const CONNECTION_POLL_OBSERVATION_GRACE_MS = 3_000;

export function boundedConnectionPollDeadline(value: string, now = Date.now()): number | null {
  const parsed = Date.parse(value);
  if (!Number.isFinite(parsed)) return null;
  const observationDeadline = Math.max(
    parsed + CONNECTION_POLL_OBSERVATION_GRACE_MS,
    now + CONNECTION_POLL_OBSERVATION_GRACE_MS,
  );
  return Math.min(observationDeadline, now + MAX_CONNECTION_POLL_MS);
}
