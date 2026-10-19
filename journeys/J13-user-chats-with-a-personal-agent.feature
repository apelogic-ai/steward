# Source: personal-agent UI acceptance for the next release.
@personal-agents @needs-model
Feature: J13 - User chats with a personal agent
  As a user with an active Envelope
  I want to create a personal agent and send it a message
  So that I can continue the conversation and inspect the governed Run

  Background:
    Given the optional integration "ticket system" is switched off
    And the optional integration "repository integration" is switched off
    And the optional integration "tool connections" is switched off
    And I am signed in as a user with the role "member"
    And I hold an Envelope from the template "standard"

  Scenario: Create an agent and see its reply and Run
    When I open "Agents"
    And I press "New agent"
    And I type "<run-prefix>-release-helper" into "Name"
    And I type "Reply to every message with exactly: personal agent complete" into "Instructions"
    And I choose "standard" in "Envelope"
    And I press "Create agent"
    Then I see "<run-prefix>-release-helper"
    When I type "Please respond." into "Message"
    And I press "Send message"
    Then the agent replies with "personal agent complete" within 4 min
    When I press "Open Run"
    Then the run ends as "succeeded" within 4 min
