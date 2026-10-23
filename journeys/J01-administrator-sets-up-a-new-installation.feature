# Source: user journey J1. UI text checked against the 0.3.22 release candidate.
# "member" stands for the role that should use the template.
# "I" is one person who is an administrator and also holds that role.
Feature: J1 - Administrator sets up a new installation
  As the administrator of a freshly installed product
  I want to create the first template
  So that users with a given role can get an Envelope without asking anyone

  Background:
    Given the optional integration "ticket system" is switched off
    And the optional integration "repository integration" is switched off
    And the optional integration "tool connections" is switched off
    And the installation's configuration has the coding agent "<Agent A>" with the model "<provider-a>/<model-a>"
    And the installation's configuration has the coding agent "<Agent B>" with the model "<provider-b>/<model-b>"
    And I am signed in as a user who is also an administrator
    And I choose "Admin workspace" in "Workspace view"

  # Steps 1 to 9 and "Done when".
  @smoke
  Scenario: Create the first template, then a second one from it
    Given no template exists
    Then I see in "Admin workspace":
      | Get started |
      | Members     |
      | Templates   |
      | Requests    |
      | Runs        |
      | Workflows   |
      | Settings    |
    When I open "Templates"
    Then I see "Create template"
    When I press "Create template"
    Then I see "Create envelope template"
    And I see "Sets the limits for one or more eligible member roles. This becomes revision 1."
    When I type "<run-prefix>-standard" into "Template ID"
    And I type "<run-prefix> Standard" into "Display name"
    And I type "member" into "Eligible member roles"
    # Money fields are in USD.
    And I type "1" into "Limit per run"
    And I type "10" into "Monthly limit"
    And I type "15" into "Time-to-live limit"
    And I choose "min" in "Time-to-live limit"
    And I leave "Runtime limit per month" empty
    Then I see in "Limit per run":
      | USD |
    And I see in "Monthly limit":
      | USD |
    And I see "Requests within these limits are approved automatically. Anything above goes to the approval queue."
    And I do not see "threshold"
    When I choose "<provider-a>/<model-a>" in "Models"
    And I choose "<provider-b>/<model-b>" in "Models"
    Then I see in "Tools":
      | Select all read-only |
      | Clear all            |
    And I see in "Workspace access":
      | Git     |
      | Scratch |
    And "Total size limit (optional)" is disabled
    When I tick "Git"
    And I tick "Scratch"
    And I leave "Total size limit (optional)" empty
    Then I see "Optional · deployment limit 2 GiB"
    And I see "Deployment limits also apply: 100,000 files, 1,000 generations, and 4 submodule levels."
    When I tick "Allow inline browser-authored packages"
    And I tick "Linux"
    And I press "Create template"
    Then I see "<run-prefix> Standard"
    And I see "Current revision 1"
    When I open "Templates"
    And I see in "Templates":
      | Member role          | Revision | Monthly limit | Limit per run | Time-to-live limit | Tools   |
      | <run-prefix>-standard | rev 1    | 10.00 USD     | 1.00 USD      | 15 min             |         |
    When I open "<run-prefix>-standard" in "Templates"
    And I type "<run-prefix>-timed" into "New template ID"
    And I type "60" into "Runtime limit per month"
    And I untick "Git"
    And I untick "Scratch"
    And I press "Save as new"
    When I open "Templates"
    And I see in "Templates":
      | Member role       | Revision | Monthly limit | Limit per run | Time-to-live limit | Tools   |
      | <run-prefix>-timed | rev 1    | 10.00 USD     | 1.00 USD      | 15 min             |         |
    And I open "<run-prefix>-standard" in "Templates"
    Then the field "Runtime limit per month" shows ""
    # Done when: a user with the eligible role finds the template.
    When I choose "User workspace" in "Workspace view"
    And I open "Envelopes"
    And I press "Request envelope"
    Then the drop-down "Template" offers "<run-prefix> Standard"
    And the drop-down "Template" offers "monthly limit 10.00 USD · time-to-live limit 15 min · 0 tools"

  # Steps 10 and 11. Gap: no existing case gives the texts of these two screens.
  Scenario: Check the members and the installation's limits
    When I open "Members"
    Then I see in "Members":
      | member |
    When I open "Settings"
    Then I see "2 GiB"
    And I see "100,000 files"
    And I see "1,000 generations"

  # F1. The size field is in GiB.
  Scenario Outline: A total size that is not a positive whole number is refused
    When I open "Templates"
    And I press "Create template"
    And I type "<run-prefix>-invalid-size" into "Template ID"
    And I tick "Git"
    And I type "<size>" into "Total size limit (optional)"
    And I press "Create template"
    Then I see "Choose at least one workspace type and enter a positive whole-number size, or leave the size blank."
    And the page "Templates" lists 0 new entries

    Examples:
      | size |
      | 0    |
      | abc  |
      | 1.5  |

  Scenario: A template that cannot be saved names the field to correct
    When I open "Templates"
    And I press "Create template"
    And I type a value the installation does not accept into "Template ID"
    And I press "Create template"
    Then the message names "Template ID"
    And the page "Templates" lists 0 new entries
