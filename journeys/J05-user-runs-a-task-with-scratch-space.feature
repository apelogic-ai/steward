# Source: user journey J5. Checked against version 0.3.16.
# The test installation gives scratch space in a fixed 2 GiB volume.
Feature: J5 - User runs a Task with scratch space
  As a user with an Envelope that allows Scratch workspaces
  I want to run a Task that writes working files to scratch space
  So that I see that it worked

  Background:
    Given the optional integration "ticket system" is switched off
    And the optional integration "repository integration" is switched off
    And the optional integration "tool connections" is switched off
    And I am signed in as a user with the role "member"
    And I hold an Envelope from the template "standard"

  # Steps 1 to 5 and "Done when".
  # 0.3.16: FAIL at step 5 (N-04).
  @needs-model @known-failing-0.3.16
  Scenario: Run a Task that writes to scratch space and reads it back
    When I open "Tasks"
    And I press "New Task"
    And I type "Scratch check" into "Name"
    And I choose "standard" in "Envelope"
    And I type "Write a 64 MiB file to the scratch folder, read it back and print the number of bytes." into "Prompt"
    # Gap: the Markdown names no way to ask for scratch space without typing this.
    And I type '[{"scratch": {"name": "scratch", "size": "512Mi"}}]' into "Workspace (advanced JSON array)"
    And I press "Save version 1"
    And I press "Run"
    And I choose "standard" in "Envelope"
    And I tick "Capture execution log"
    And I press "Run now"
    Then I do not see "The run request was rejected."
    # The Task gets the whole fixed volume.
    And I see in "Workspace":
      | scratch                   |
      | Scratch                   |
      | Writable · capped at 2Gi |
    And the run ends as "succeeded" within 4 min
    # 67,108,864 bytes are 64 MiB, written and read back.
    And the run output contains "67108864"

  # F1, F2 and the first case of F3.
  # 0.3.16: the decision is right, but each text is wrapped (N-03).
  @known-failing-0.3.16
  Scenario Outline: Scratch space the Envelope cannot give is refused: <case>
    Given I hold an Envelope from the template "<template>"
    When I open "Tasks"
    And I press "New Task"
    And I type "Scratch check" into "Name"
    And I choose "<template>" in "Envelope"
    And I type '[{"scratch": {"name": "scratch", "size": "<size>"}}]' into "Workspace (advanced JSON array)"
    And I press "Save version 1"
    And I press "Run"
    And I choose "<template>" in "Envelope"
    And I press "Run now"
    Then I see "The run request was rejected."
    And I see "<sentence>"
    And I do not see 'Admission("'
    And the page "Runs" lists 0 new entries

    Examples:
      | case                                | template    | size  | sentence                                                                                                            |
      | 4 GiB asked, 2 GiB allowed          | standard    | 4Gi   | Total scratch size 4 GiB is more than the 2 GiB this Envelope allows.                                               |
      | 1 GiB allowed, fixed 2 GiB volume   | standard-1g | 512Mi | This deployment provides scratch space in a fixed 2 GiB volume, which is more than the 1 GiB this Envelope allows. |
      | Scratch not allowed by the Envelope | git-only    | 512Mi | Scratch workspaces are not allowed by this Envelope. Allowed workspace types: Git.                                  |

  # F3, second case.
  # 0.3.16: the decision is right, but the text is wrapped (N-03).
  @known-failing-0.3.16
  Scenario: Scratch space that uses the whole allowance next to a Git entry is refused
    When I open "Tasks"
    And I press "New Task"
    And I type "Scratch and Git" into "Name"
    And I choose "standard" in "Envelope"
    And I type '[{"git": {"repository": "<owner>/<repository>", "ref": "git:ref:main"}}, {"scratch": {"name": "scratch", "size": "2Gi"}}]' into "Workspace (advanced JSON array)"
    And I press "Save version 1"
    And I press "Run"
    And I press "Run now"
    Then I see "The run request was rejected."
    And I see "Git workspaces need available capacity, but scratch requests use all 2 GiB this Envelope allows."
    And I do not see 'Admission("'
    And the page "Runs" lists 0 new entries
