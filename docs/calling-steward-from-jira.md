# Calling Steward from Jira webhook Triggers

Status: **Current Steward v0.3.26 contract**

A Jira webhook Trigger starts one governed Workflow when a signed Jira event
matches deployment-owned settings. The receiver is a separate, optional service.
It does not accept identity, Workflow, Envelope, or service choices from the
webhook body, and it never writes to Jira.

The Helm chart is the configuration contract. See the
[Jira Trigger receiver chart reference](../charts/steward/README.md#jira-trigger-receiver)
for every prerequisite, Secret and network setting.

## Before you enable a Trigger

Prepare all of the following:

- a published Workflow revision, written as `name@N`;
- the stable service name and canonical owner that the receiver's short-lived
  Task token represents;
- the exact active User Envelope digest for that owner;
- a deployment capability catalog that classifies every tool in that Envelope
  as `read`;
- an existing Kubernetes Secret containing the Jira webhook signing secret;
- the release's immutable `images.triggerReceiver` image, using the
  `<version>-trigger-receiver` tag and its matching digest;
- a separate, read-only database credential for Envelope lookup;
- the workload-exchange and Task API HTTPS trust settings; and
- an HTTPS Gateway or Ingress route that publishes only the exact Trigger path.

The receiver checks the Envelope when it starts and again immediately before
each submission. An absent, stale, ambiguous, unknown, write-capable or
destructive tool grant is refused. Changing an Envelope or the capability
catalog therefore cannot turn an already configured Trigger into a write path.

## Configure the Trigger

This example shows the Trigger-specific values. Keep the remaining receiver,
trust, network and limit values described by the chart reference.

```yaml
triggerReceiver:
  enabled: true
  databaseSecret: { name: steward-trigger-database, key: url }
  workloadExchange:
    endpoint: https://identity.identity-exchange.svc.cluster.local:8443/v1/workload/exchange
    serverName: identity.identity-exchange.svc.cluster.local
    audience: apelogic-workload-exchange
  taskApi:
    ca: { kind: ConfigMap, name: steward-apiserver-ca, key: ca.crt }
  triggers:
    - id: jira_read
      enabled: true
      source: jira
      workflow: triage@1
      service: event-worker
      ownerUserId: usr_11111111111111111111111111111111
      envelopeDigest: steward:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
      filters:
        event: jira:issue_updated
        project: PROJ
        issueType: "10001"
      ignoredAccountIds: []
      concurrencyLimit: 2
      cooldownSeconds: 60
      signingSecret: { name: steward-jira-webhook, key: secret }
networkPolicy:
  ingressNamespace: edge-system
  kubeApiCidrs: [192.0.2.1/32]
  postgresCidrs: [192.0.2.2/32]
```

The Trigger fields have these meanings:

| Value | What it controls |
|---|---|
| `id` | Stable Trigger name used in the public route and replay key. |
| `enabled` | Whether the exact Trigger route is available. |
| `source` | Must be exactly `jira`. |
| `workflow` | Exact published Workflow revision to run. |
| `service` | Stable service recorded for runs started by this receiver. It must match the workload-exchange configuration. |
| `ownerUserId` | Canonical owner whose active Envelope is selected. |
| `envelopeDigest` | Exact active User Envelope. Every tool grant must be classified as read-only. |
| `filters.event` | Optional exact Jira webhook event, such as `jira:issue_updated`. |
| `filters.project` | Optional exact Jira project key. |
| `filters.issueType` | Optional exact Jira issue-type ID. |
| `ignoredAccountIds` | Jira account IDs whose matching events return successfully without starting a run. Use this as a loop guard. |
| `concurrencyLimit` | Limit on nonterminal runs held by this Trigger. |
| `cooldownSeconds` | Minimum delay before the same issue can start another run. Use `0` to disable the delay. |
| `signingSecret` | Existing Secret name and key whose raw bytes Jira also uses to sign the webhook. The value is mounted read-only and is never copied into Helm values. |

Filters are exact matches and combine with AND. A filtered or ignored event is a
successful no-op; it is not an error and does not create a Task.

For one Trigger, semantically identical JSON bodies share one replay key even
when whitespace or object-field order differs. A retry after acceptance returns
`200` and does not start a second Task. The Task API stores that key in PostgreSQL,
so this remains true when a retry reaches another receiver process or the first
process restarted. A duplicate does not consume the receiver's cooldown or
concurrency limit. A changed event body is a new delivery.

## Configure the Jira webhook

For a Trigger whose `id` is `jira_read`, configure Jira to send the selected
events to:

```text
https://steward.example.com/hooks/jira/jira_read
```

The route is always `/hooks/jira/<trigger>`. Publish that exact path only. Do
not publish the receiver root or `/hooks/jira` as a prefix.

Configure the same secret bytes in Jira and in the referenced Kubernetes
Secret. Jira must send `X-Hub-Signature` as `sha256=` followed by the lowercase
hexadecimal HMAC-SHA256 of the untouched request body. A proxy must not rewrite
the body before the receiver checks the signature.

The receiver copies only bounded event kind, issue key, project key, issue-type
ID and author account ID fields into `in/event.json`. Jira summary, description
and comment text do not become Workflow instructions.

## Response meanings

| Status | Meaning | Caller action |
|---|---|---|
| `202 Accepted` | A new Task was accepted and its lifecycle started. | Do not resend unless Jira is retrying an uncertain delivery. |
| `200 OK` | No new run was needed: the event was filtered, ignored or already accepted. | Treat the delivery as complete. |
| `400 Bad Request` | The event, configured Envelope or Task request cannot be used. | Correct the event or deployment settings; retries without a change will not help. |
| `401 Unauthorized` | The signing header is missing, malformed or does not match the raw body. | Correct the webhook secret or any body-changing proxy. |
| `404 Not Found` | The Trigger ID is unknown or disabled. | Correct the exact URL or enable the Trigger. |
| `413 Payload Too Large` | The request exceeds the configured body limit. | Reduce the Jira payload or review the deployment limit. |
| `429 Too Many Requests` | The Trigger has reached its run limit, the issue is cooling down or the same delivery is still being processed. | Retry later. |
| `503 Service Unavailable` | A required service could not complete the request. | Retry with backoff. |

The receiver never transitions an issue, adds a comment or otherwise writes
back to Jira. These HTTP responses are the complete webhook acknowledgement.

## Optional issue link in run views

Set `config.apiserver.jiraBaseUrl` to the HTTPS Jira site URL if run views
should link back to the issue. An optional Jira context path is allowed. When
the value is absent, the run shows a Jira Trigger label without a link when
that label fits the caller-reference limit; otherwise the reference is
omitted.

A bounded issue key that does not match the expected Jira key form or project
is display metadata only: delivery continues, and the receiver falls back to a
bounded Trigger label without a URL when possible. Empty, control-containing or
over-limit issue keys are still refused with the rest of the invalid event. Do
not add a second site value under `triggerReceiver`; the receiver uses the
deployment's existing Jira site setting.

## Deployment acceptance

Local-main deployment acceptance passed on Steward revision `e37e7c54`
([acceptance evidence](https://github.com/apelogic-ai/steward/issues/524#issuecomment-6246865924)).
The exact Trigger route refused an unsigned delivery with `401`. A fresh signed
delivery returned `202` and created exactly one new AgentRuntime. Replaying the
same signed delivery returned `200` and created no new Task.
