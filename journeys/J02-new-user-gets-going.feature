# Source: user journey J2. UI text exercised against the 0.3.17 release candidate.
# "member" stands for the role the template is meant for.
Feature: J2 - New user gets going
  As a user who signs in for the first time
  I want to get an Envelope from a template and run a first Task
  So that I see the result and what it cost

  Background:
    Given the optional integration "ticket system" is switched off
    And the optional integration "repository integration" is switched off
    And the optional integration "tool connections" is switched off
    And the template "standard" exists
    And I am signed in as a user with the role "member"

  # All steps and "Done when".
  @smoke @needs-model
  Scenario: Get an Envelope, run a first Task and see what it cost
    Given I hold no Envelope
    # Gap: no text is given for what a first sign-in shows besides this entry.
    Then I see "Get started"
    When I open "Envelopes"
    And I press "Request envelope"
    Then the drop-down "Template" offers "rev 1 — monthly limit 10.00 USD · time-to-live limit 15 min · 0 tools"
    # "Custom envelope" is the last entry.
    And the drop-down "Template" offers "Custom envelope"
    When I choose "standard" in "Template"
    Then I see "01 Budget and lifetime"
    And I see "02 Models"
    And I see "03 Tools"
    And I see "04 Workspace access"
    And I see "05 Request note"
    And I see "Limit 10.00 USD"
    And I see "Limit 1.00 USD"
    And I see "Limit 15 min"
    And I see in "Summary":
      | Template           |          |
      | Monthly limit      |          |
      | Limit per run      |          |
      | Time-to-live limit |          |
      | Models             | 2 models |
      | Tools              | 0 tools  |
    And I see "This request is within the template limits and will be provisioned immediately."
    # The field is in USD.
    When I type "0.50" into "Limit per run"
    # Keeps this Envelope distinct from active Envelopes left by earlier smoke runs.
    And I type "<run-runtime-minutes>" into "Runtime limit per month"
    And I press "Submit request"
    # Provisioned at once: no administrator acts in this scenario.
    Then I see the status "provisioned"
    And I see "Limit per run 0.50 USD"
    And I see in "Approved access":
      | Total size limit | Deployment limit · 2 GiB             |
      | File limit       | Deployment limit · 100,000 files     |
      | History depth    | Deployment limit · 1,000 generations |
      | Submodule depth  | Deployment limit · 4 levels          |
    And I do not see "undefined"
    And I do not see "null"
    And the page "Envelopes" lists 1 new entry
    When I open "Runs"
    And I press "Run now"
    And I choose "standard" in "Envelope"
    And I choose "Inline package" in "Package source"
    And I tick "Capture execution log"
    # A coding agent and a model that this Envelope allows are selected.
    Then "Run now" is enabled
    When I press "Run now"
    Then the run ends as "succeeded" within 2 min
    And the run output contains "hello world"
    And the spend of the run is above 0.00 USD
    And every amount of money on the page has 2 decimals and the unit "USD"
    And the page "Runs" lists 1 new entry
    When I open "Envelopes"
    # For example "0.02 USD / 10.00 USD".
    Then the Envelope from the template "standard" shows the same spend as used out of its monthly limit

  # F1. The monthly limit of the template is 10.00 USD.
  Scenario Outline: A limit per run that is missing or too high cannot be submitted
    When I open "Envelopes"
    And I press "Request envelope"
    And I choose "standard" in "Template"
    And I type "<limit per run in USD>" into "Limit per run"
    Then I see "<message>"
    And "Submit request" is disabled

    Examples:
      | limit per run in USD | message                                        |
      |                      | Enter a limit per run.                         |
      | 11                   | Limit per run cannot exceed the monthly limit. |

  # F2.
  Scenario: A coding agent that cannot work with the Envelope is greyed out
    Given the installation's configuration has the coding agent "<Agent A>" with the model "<provider-a>/<model-a>"
    And the installation's configuration has the coding agent "<Agent B>" with the model "<provider-b>/<model-b>"
    And I hold an Envelope from the template "basic"
    When I open "Runs"
    And I press "Run now"
    And I choose "basic" in "Envelope"
    Then "<Agent B>" is disabled
    And I see "This Envelope does not allow a model supported by this coding agent."
    And the field "Coding agent" shows "<Agent A>"
    And "Run now" is enabled

  # F3. 0.3.16: the run costs more than its limit per run and succeeds (N-06).
  # The Markdown also accepts: the run is parked and an administrator sees
  # "Escalated: a cumulative limit was reached and the run is parked".
  @needs-model @known-failing-0.3.16
  Scenario: A run that reaches its limit per run is stopped
    When I open "Envelopes"
    And I press "Request envelope"
    And I choose "standard" in "Template"
    # 0.01 USD.
    And I type "0.01" into "Limit per run"
    And I press "Submit request"
    And I start a Hello world run with the Envelope from the template "standard"
    Then the run stops with a message about its limit per run
