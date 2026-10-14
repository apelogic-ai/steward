# Source: user journey J6. Original product findings were recorded against version 0.3.16.
# The "basic" Envelope narrows the run's standard template to one model and no workspace.
# The "timed" Envelope allows 60 min of runtime per month.
Feature: J6 - User asks for more than their Envelope allows in a Task
  As a user who writes a Task that states its own needs
  I want to learn before anything starts that the Task asks for too much
  So that I know what was asked, what the limit is and what to do

  Background:
    # No tool is called in this journey.
    Given the optional integration "ticket system" is switched off
    And the optional integration "repository integration" is switched off
    And I am signed in as a user with the role "member"
    And I hold an Envelope from the template "basic"
    And I hold an Envelope from the template "timed"

  # Steps 1 and 2. Every below-limit budget remains positive.
  @extended @needs-model
  Scenario: Needs at or below the Envelope's limits are accepted
    When I open "Tasks"
    And I press "New Task"
    And I type "Stated needs" into "Name"
    And I choose "basic" in "Envelope"
    And I type "Write the single line hello world to out/hello.txt" into "Prompt"
    And I state requirements that are all below the Envelope's limits
    And I press "Save version 1"
    And I press "Run"
    And I press "Run now"
    Then the run ends as "succeeded" within 2 min

    When I open "Tasks"
    And I open "Stated needs" in "Tasks"
    Then I see in "Access":
      | Declared by this Task |
    # No value is refused for being equal, and none has to match exactly.
    When I press "Edit into v2"
    And I type "Write the single line hello world to out/hello.txt" into "Prompt"
    And I state requirements that are all at the Envelope's limits
    And I press "Save version 2"
    And I press "Run"
    And I press "Run now"
    Then the run ends as "succeeded" within 2 min

  @extended
  Scenario: A zero per-run budget is refused when the Task is saved
    When I open "Tasks"
    And I press "New Task"
    And I type "Zero per-run budget" into "Name"
    And I choose "basic" in "Envelope"
    And I state a per-run budget of zero
    And I press "Save version 1"
    Then I see "The per-run budget must be more than 0.00 USD."
    And I see "Save version 1"

  # Steps 3 and 4 and "Done when". Failure path F2 is checked here too:
  # every refusal must say what to do next.
  # The sentences are the EXPECTED plain wording. 0.3.16 shows
  # 'The run request was rejected. Admission("...")' with "$25" and "20m" (N-03).
  # Specification only: the historical release label is not a dedicated issue identity.
  Scenario Outline: A need above the Envelope's limit is refused before anything starts: <requirement>
    When I open "Tasks"
    And I press "New Task"
    And I type "Stated needs" into "Name"
    And I choose "<template>" in "Envelope"
    And I state a requirement above the Envelope's limit: "<requirement>"
    And I press "Save version 1"
    And I press "Run"
    And I press "Run now"
    Then I see "The run request was rejected."
    And I see "<sentence>"
    And the message says what to do next
    And I do not see 'Admission("'
    And the page "Runs" lists 0 new entries

    Examples:
      | template | requirement                                  | sentence                                                                                        |
      | basic    | the tool github:get_file_contents:read       | Tool github:get_file_contents:read is not one of the tools this Envelope allows.                |
      | basic    | a monthly budget of 25.00 USD                | Monthly budget 25.00 USD is more than the 20.00 USD this Envelope allows.                       |
      | basic    | a per-run budget of 1.00 USD                 | Per-run budget 1.00 USD is more than the 0.75 USD this Envelope allows.                         |
      | basic    | a per-run budget with no limit               | A per-run budget with no limit is more than the 0.75 USD this Envelope allows.                  |
      | basic    | a maximum run duration of 20 min             | Maximum run duration 20 min is more than the 15 min this Envelope allows.                       |
      | basic    | the runner platform Windows                  | Runner platform Windows is not one of the platforms this Envelope allows.                       |
      | timed    | a runtime allowance of 90 min                | Runtime allowance 90 min is more than the 60 min per month this Envelope allows.                |
      | basic    | a model the Envelope does not allow          | Choose an agent compatible with this Envelope, or request an Envelope that allows its model.    |

  # F1. 0.3.16: "Steward rejected this package. Check its paths, Task definition,
  # skills, workspace, outputs, and requirements." (J6 F1; no finding number).
  # No dedicated issue defines the field-level invalid-Requirements message to verify.
  @known-failing-407
  Scenario: Requirements that are not valid are refused with what is wrong
    When I open "Tasks"
    And I press "New Task"
    And I type "Stated needs" into "Name"
    And I choose "basic" in "Envelope"
    And I type "{" into "Requirements (optional JSON)"
    And I press "Save version 1"
    Then the message names "Requirements"
    And the page "Tasks" lists 0 new entries
