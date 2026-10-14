# Source: user journey J8. Original product findings were recorded against version 0.3.16.
# "<Agent A>" is the name shown for an agent; "<agent-b>" is the short name shown after "·".
Feature: J8 - User chooses agent and model
  As a user with Envelopes that allow different models
  I want to pick a coding agent and a model that work together under the chosen Envelope
  So that a saved Task keeps the choice I made

  Background:
    Given the optional integration "ticket system" is switched off
    And the optional integration "repository integration" is switched off
    And the optional integration "tool connections" is switched off
    And the installation's configuration has the coding agent "<Agent A>" with the model "<provider-a>/<model-a>"
    And the installation's configuration has the coding agent "<Agent B>" with the model "<provider-b>/<model-b>"
    And I am signed in as a user with the role "member"
    # "broad" allows both models, "basic" allows model A only.
    And I hold an Envelope from the template "broad"
    And I hold an Envelope from the template "basic"

  # Steps 1 to 7 and "Done when". PASS on 0.3.16.
  @extended @needs-model
  Scenario: Pick agent and model, run, and keep the choice in a saved Task
    When I open "Runs"
    And I press "Run now"
    And I choose "broad" in "Envelope"
    Then the drop-down "Coding agent" offers "<Agent A>"
    And the drop-down "Coding agent" offers "<Agent B>"
    When I choose "<Agent A>" in "Coding agent"
    # With one agent the Model list holds only the model of that agent.
    Then the drop-down "Model" offers "<provider-a>/<model-a>"
    And I do not see "<provider-b>/<model-b>" in "Model"
    When I choose "<Agent B>" in "Coding agent"
    Then the drop-down "Model" offers "<provider-b>/<model-b>"
    And I do not see "<provider-a>/<model-a>" in "Model"
    When I choose "basic" in "Envelope"
    Then "<Agent B>" is disabled
    And the disabled agent "<Agent B>" says "This Envelope does not allow a model supported by this coding agent."
    And I see the text fragment "is not compatible with this Envelope."
    And I see the text fragment "you can choose another."
    And the field "Coding agent" shows "<Agent A>"
    And "Run now" is enabled
    When I choose "broad" in "Envelope"
    And I choose "<Agent B>" in "Coding agent"
    And I choose "<provider-b>/<model-b>" in "Model"
    And I press "Run now"
    Then the run ends as "succeeded" within 2 min
    And the run records coding agent "<agent-b>" and model "<provider-b>/<model-b>"
    When I open "Tasks"
    And I press "New Task"
    And I type "Agent choice" into "Name"
    And I choose "broad" in "Envelope"
    And I choose "<Agent B>" in "Coding agent"
    And I choose "<provider-b>/<model-b>" in "Model"
    And I press "Save version 1"
    And I press "Edit into v2"
    And I choose "basic" in "Envelope"
    Then the field "Coding agent" shows "Keep current: <agent-b>"
    And the field "Model" shows "Keep current: <provider-b>/<model-b>"
    # The page also notes that the agent and the model are not available with the
    # selected Envelope and can be kept for this draft. No exact text is given.
    When I press "Save version 2"
    Then the field "Coding agent" shows "<agent-b>"
    And the field "Model" shows "<provider-b>/<model-b>"
    When I press "Edit into v3"
    And I choose "<Agent A>" in "Coding agent"
    Then the field "Model" shows "<provider-a>/<model-a>"
    And I do not see "Keep current"
    When I open "Get started"
    And I press "Try a test run"
    And I choose "broad" in "Envelope"
    Then the drop-down "Coding agent" offers "<Agent B>"
    When I choose "basic" in "Envelope"
    Then "<Agent B>" is disabled
    And the disabled agent "<Agent B>" says "unavailable"

  # F1. 0.3.16: refused and nothing is created, but the text is wrapped and the
  # sentence that says what to do is not shown (N-03).
  Scenario: A saved Task whose model the Envelope does not allow is refused
    Given I have a saved Task named "Agent choice" with the coding agent "<Agent B>" and the model "<provider-b>/<model-b>"
    When I open "Tasks"
    And I open "Agent choice" in "Tasks"
    And I press "Run"
    And I choose "basic" in "Envelope"
    And I press "Run now"
    Then I see "The run request was rejected. Choose an agent compatible with this Envelope, or request an Envelope that allows its model."
    And I do not see 'Admission("'
    And the page "Runs" lists 0 new entries

  # F2. Text seen on 0.3.16; no case covers this state on its own.
  # "no-agent" stands for a template whose only model works with no configured agent.
  Scenario: An Envelope whose models work with no agent cannot start a run
    Given I hold an Envelope from the template "no-agent"
    When I open "Runs"
    And I press "Run now"
    And I choose "no-agent" in "Envelope"
    Then the field "Model" shows "No compatible model"
    And "Run now" is disabled
