# Source: user journey J10. Original product findings were recorded against version 0.3.16.
# "I" is the operator, who works outside the web UI. The checks in the UI are
# done by "an administrator" and "a user". Suited to a release candidate, not to every build.
@operator @two-accounts @nightly
Feature: J10 - Operator changes configuration
  As the operator who installs and upgrades the product
  I want to upgrade and to add a model by configuration alone
  So that nothing users already have is broken

  Background:
    # Everything below exists from the previous version.
    Given the optional integration "ticket system" is switched off
    And the optional integration "repository integration" is switched off
    And the optional integration "tool connections" is switched off
    And the template "standard" exists
    And an administrator is signed in on a separate account
    And an administrator chooses "Admin workspace" in "Workspace view"
    And a user is signed in on a separate account
    And a user holds an Envelope from the template "standard"
    And a user has a saved Task named "Before the upgrade" with the coding agent "<Agent A>" and the model "<provider-a>/<model-a>"

  # Part A, steps 1 to 4. Steps 1 and 2 pass on 0.3.16; step 4 was not exercised.
  @needs-model
  Scenario: Part A - after an upgrade, what users had still works
    When I upgrade the installation to the new version
    Then every service is running with 0 restarts and 0 error lines
    When an administrator opens "Templates"
    And an administrator opens "standard" in "Templates"
    # One set of limits per template.
    Then an administrator sees "Monthly limit"
    And an administrator sees "Limit per run"
    And an administrator does not see "threshold"
    When a user opens "Envelopes"
    And a user opens "standard" in "Envelopes"
    Then a user sees the status "provisioned"
    And a user sees "Time-to-live limit 15 min"
    When a user starts a Hello world run with the Envelope from the template "standard"
    Then the run ends as "succeeded" within 2 min
    When a user opens "Tasks"
    And a user opens "Before the upgrade" in "Tasks"
    Then the field "Coding agent" shows "<Agent A>"
    And the field "Model" shows "<provider-a>/<model-a>"
    When a user presses "Run"
    And a user presses "Run now"
    Then the run ends as "succeeded" within 2 min

  # Part B, steps 5 to 8 and "Done when".
  # 0.3.16: BLOCKED at step 5 (case M-06; no finding number).
  @needs-model @known-failing-407
  Scenario: Part B - a model added only in configuration can be picked and run
    When I add the model "<provider-a>/<new-model>" to the installation's configuration without a new product build
    Then every service is running with 0 restarts and 0 error lines
    When an administrator opens "Templates"
    And an administrator opens "standard" in "Templates"
    Then the drop-down "Models" offers "<provider-a>/<new-model>"
    When an administrator chooses "<provider-a>/<new-model>" in "Models"
    And an administrator presses "Save new version"
    Then an administrator sees "Template revision saved."
    When a user opens "Envelopes"
    And a user presses "Request envelope"
    And a user chooses "standard" in "Template"
    Then a user sees in "02 Models":
      | <provider-a>/<new-model> |
    When a user presses "Submit request"
    Then a user sees the status "provisioned"
    When a user opens "Runs"
    And a user presses "Run now"
    And a user chooses "standard" in "Envelope"
    And a user chooses "<Agent A>" in "Coding agent"
    Then the drop-down "Model" offers "<provider-a>/<new-model>"
    When a user chooses "<provider-a>/<new-model>" in "Model"
    And a user presses "Run now"
    Then the run ends as "succeeded" within 2 min

  # F1. Gap: no existing case. "<provider-c>/<model-c>" stands for a model that
  # no configured agent can use. The Markdown also accepts: shown as unavailable with the reason.
  Scenario: A configured model that no agent can use is not offered
    When I add the model "<provider-c>/<model-c>" to the installation's configuration without a new product build
    And a user opens "Runs"
    And a user presses "Run now"
    And a user chooses "standard" in "Envelope"
    Then a user does not see "<provider-c>/<model-c>" in "Model"

  # F2. Not observable from the UI; an operator check.
  Scenario: A configuration that is not valid for this version is refused
    When I apply a configuration that is not valid for this version
    Then the services refuse to start with a message that names the setting
