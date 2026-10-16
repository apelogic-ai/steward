# Source: Jira governed-tool acceptance for version 0.3.19.
# The owner completes Atlassian sign-in before this journey; the saved Playwright state remains localhost-only.
@needs-jira
Feature: J11 - User runs a Task with Jira
  As a user with a connected Jira account
  I want to approve one Jira read tool for a Task
  So that the agent can read Jira without receiving broader tool authority

  Background:
    Given the optional integration "ticket system" is switched off
    And the optional integration "repository integration" is switched off
    And the optional integration "tool connections" is switched on
    And I am signed in as a user with the role "member"
    And I hold an Envelope from the template "jira-read"

  @needs-model
  Scenario: Run a Task with exactly one Jira read tool
    When I open "Connections"
    Then I see in "Jira":
      | connected |
    When I open "Tasks"
    And I press "New Task"
    And I type "Read Jira" into "Name"
    And I choose "jira-read" in "Envelope"
    And I type "Use the approved Jira search tool to list accessible issues. After a successful Jira tool call, write jira read succeeded to out/jira.txt. Do not write that file unless the tool call succeeds." into "Prompt"
    And I state requirements that are all at the Envelope's limits
    And I press "Save version 1"
    And I press "Run"
    And I choose "jira-read" in "Envelope"
    And I press "Run now"
    Then the run ends as "succeeded" within 4 min
    And the run output contains "jira read succeeded"
