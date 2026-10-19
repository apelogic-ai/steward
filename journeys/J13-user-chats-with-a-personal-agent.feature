# Source: personal-agent UI acceptance for the next release.
@personal-agents @needs-model
Feature: J13 - User chats with a personal agent
  As a user who can create an Envelope
  I want to create a personal agent and send it a message
  So that I can continue the conversation and inspect the governed Run

  Background:
    Given the optional integration "ticket system" is switched off
    And the optional integration "repository integration" is switched off
    And the optional integration "tool connections" is switched off
    And the installation's configuration has the coding agent "<Agent A>" with the model "<provider-a>/<model-a>"
    And the installation's configuration has the coding agent "<Agent B>" with the model "<provider-b>/<model-b>"
    And I am signed in as a user who is also an administrator
    And I choose "Admin workspace" in "Workspace view"
    When I open "Templates"
    And I press "Create template"
    And I type "<run-prefix>-standard" into "Template ID"
    And I type "<run-prefix> Standard" into "Display name"
    And I type "member" into "Eligible member roles"
    And I type "1" into "Limit per run"
    And I type "10" into "Monthly limit"
    And I type "15" into "Time-to-live limit"
    And I choose "min" in "Time-to-live limit"
    And I leave "Runtime limit per month" empty
    And I choose "<provider-a>/<model-a>" in "Models"
    And I choose "<provider-b>/<model-b>" in "Models"
    And I tick "Git"
    And I tick "Scratch"
    And I leave "Total size limit (optional)" empty
    And I tick "Allow inline browser-authored packages"
    And I tick "Linux"
    And I press "Create template"
    Then I see "<run-prefix> Standard"
    When I choose "User workspace" in "Workspace view"
    And I hold an Envelope from the template "standard"

  Scenario: Create an agent and see its reply and Run
    When I open "My agents"
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
