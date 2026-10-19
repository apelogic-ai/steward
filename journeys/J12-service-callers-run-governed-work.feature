# Source: caller acceptance for the upcoming 0.3.20 release.
# The direct receiver scenarios use the deployed Trigger contract. The live path remains owner-only.
@callers
Feature: J12 - Service callers run governed work
  As an automation owner
  I want Jenkins, Temporal and Jira to start governed work through their bound identities
  So that automation can read its result without receiving broader access

  Background:
    Given the caller journey target is configured

  @needs-model
  Scenario: A Jenkins job exchanges its identity and reads Run API output
    Given the "Jenkins" caller is bound to a versioned Workflow
    When a Jenkins job exchanges its source assertion
    And it runs the bound Workflow with the stable identity "jenkins:<run-prefix>:101"
    Then the caller Task ends as "succeeded" within 4 min
    And the caller downloads output containing "caller journey complete"

  @needs-model
  Scenario: A Temporal activity exchanges its workload identity and reads Run API output
    Given the "Temporal" caller is bound to a versioned Workflow
    When a Temporal activity exchanges its workload assertion
    And it runs the bound Workflow with the stable identity "default:<run-prefix>:run-123:activity-123"
    Then the caller Task ends as "succeeded" within 4 min
    And the caller downloads output containing "caller journey complete"

  Scenario: An unbound caller cannot obtain a Task token
    Given an unbound caller assertion is available
    When the unbound caller requests a Task token
    Then the token exchange is refused
    And the caller creates 0 new Tasks

  Scenario: A signed Jira webhook starts one governed Task
    Given the Jira receiver uses a read-only Envelope
    When Jira sends the signed delivery "delivery-123" for issue "PROJ-123"
    Then the receiver accepts the delivery
    And the receiver creates 1 new Task
    And the receiver Task ends as "succeeded" within 4 min

  # Covered by bins/steward-trigger-receiver/tests/write_catalog_process.rs, which proves both
  # startup and live-reload refusal with the real receiver and PostgreSQL.
  @covered-by-process-e2e
  Scenario: A write-capable Envelope is refused by the Jira receiver
    Given the Jira receiver uses a write-capable Envelope
    When Jira sends the signed delivery "delivery-456" for issue "PROJ-124"
    Then the receiver refuses the delivery because the Envelope is not read-only
    And the receiver creates 0 new Tasks

  Scenario: Replaying a signed Jira delivery creates no second Task
    Given the Jira receiver uses a read-only Envelope
    When Jira sends the signed delivery "delivery-789" for issue "PROJ-125"
    And Jira replays that signed delivery
    Then the receiver reports that no run was needed
    And the receiver creates 1 new Task
    And the receiver Task ends as "succeeded" within 4 min

  Scenario: A Jira webhook with an invalid signature is refused
    Given the Jira receiver uses a read-only Envelope
    When Jira sends the delivery "delivery-invalid-signature" for issue "PROJ-126" with an invalid signature
    Then the receiver refuses the delivery because its signature is invalid
    And the receiver creates 0 new Tasks

  Scenario: A Jira webhook without a signature is refused
    Given the Jira receiver uses a read-only Envelope
    When Jira sends the delivery "delivery-missing-signature" for issue "PROJ-127" without a signature
    Then the receiver refuses the delivery because its signature is invalid
    And the receiver creates 0 new Tasks

  Scenario: A Jira Trigger enforces its per-issue cooldown
    Given the Jira receiver uses a read-only Envelope
    When Jira sends the signed delivery "delivery-cooldown-first" for issue "PROJ-128"
    And Jira sends the signed delivery "delivery-cooldown-second" for issue "PROJ-128"
    Then the receiver reports that it cannot start another run yet
    And the receiver creates 1 new Task
    And the receiver Task ends as "succeeded" within 4 min

  @live-jira-webhook @needs-public-ingress @owner-only
  Scenario: A live Jira webhook reaches the public receiver
    Given the owner explicitly allowed a live Jira webhook test
    And the Jira receiver uses a read-only Envelope
    When the live Jira helper updates issue "PROJ-129"
    Then the receiver creates 1 new Task
    And the receiver Task ends as "succeeded" within 4 min
