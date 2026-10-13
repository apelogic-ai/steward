# Source: user journey J4. UI text aligned with the 0.3.17 release candidate.
# This journey is about the repository integration, so it is switched on.
@needs-github
Feature: J4 - User runs a Task on a repository
  As a user with an Envelope that allows Git workspaces
  I want to run a Task on a named repository and branch
  So that I see exactly which commit was used and can run the same package again

  Background:
    Given the optional integration "ticket system" is switched off
    And the optional integration "tool connections" is switched off
    And the optional integration "repository integration" is switched on
    And the repository "<owner>/<repository>" is allowed in this installation
    And I am signed in as a user with the role "member"
    And I hold an Envelope from the template "standard"

  # Steps 1 to 6.
  # The runtime does not find the checked-out repository yet (#464).
  @smoke @needs-model @known-failing-464
  Scenario: Run a Task on a named repository and branch
    When I open "Tasks"
    And I press "New Task"
    And I type "Read the repository" into "Name"
    And I choose "standard" in "Envelope"
    And I type "List the files in the top folder of the repository and write them to files.txt." into "Prompt"
    Then the drop-down "Workspace repository" offers "No repository checkout"
    And the drop-down "Workspace repository" offers "<owner>/<repository>"
    When I choose "<owner>/<repository>" in "Workspace repository"
    Then the field "Branch or tag" shows "main"
    # The repository is shown by name, not by a number.
    And the field "Workspace (advanced JSON array)" shows '[{"git": {"repository": "<owner>/<repository>", "ref": "git:ref:main"}}]'
    When I open "Review exact Task definition"
    Then I see in "Review exact Task definition":
      | Read the repository |
      | main                |
    When I press "Save version 1"
    And I press "Run"
    # The run form preselects the newest Envelope. The person must check it.
    And I choose "standard" in "Envelope"
    And I tick "Capture execution log"
    And I press "Run now"
    Then I do not see "The run request was rejected."
    And I see in "Workspace":
      | <owner>/<repository> |
      | Workspace entry:     |
      | Git · read-only      |
      | files only           |
    When I open "Repository details"
    Then I see in "Repository details":
      | Owner ID      |
      | Repository ID |
      | Exact commit  |
    And "Exact commit" shows a commit of 40 hex characters that is the head of the branch "main"
    And the run ends as "succeeded" within 4 min
    # "README" stands for a file known to be in the top folder of the repository.
    And the run output contains "README"

  # Step 7.
  # 0.3.16: fails the same way as step 4 (N-02).
  @nightly @needs-model @known-failing-0.3.16
  Scenario: Run now on the default repository with a history depth
    When I open "Runs"
    And I press "Run now"
    And I tick "Check out this repository"
    And I choose "Default repository at trigger revision" in "Workspace repository"
    # 3 generations.
    And I type "3" into "History depth (optional)"
    And I press "Run now"
    Then I do not see "The run request was rejected."
    And I see in "Workspace":
      | <owner>/<repository> |
      | depth 3              |
    And "Exact commit" shows a commit of 40 hex characters that is the head of the branch "main"
    And the run ends as "succeeded" within 4 min

  # Step 8. The earlier run is the run of the first scenario.
  # 0.3.16: not reached, because no repository run succeeds (N-02).
  @nightly @needs-model @known-failing-0.3.16
  Scenario: Re-run keeps the same package and commit
    Given I have a run that ended as "succeeded" with a captured execution log
    When I open "Runs"
    And I open the run that ended as "succeeded"
    And I press "Re-run"
    Then the page "Runs" lists 1 new entry
    # The same commit, if the branch has not moved.
    And "Exact commit" shows a commit of 40 hex characters that is the head of the branch "main"
    And the run ends as "succeeded" within 4 min

  # F1. 0.3.16: the text is wrapped (N-03) and does not name the repository.
  @known-failing-0.3.16
  Scenario: A repository that is not allowed is refused by name
    Given the repository "<owner>/<other-repository>" is unavailable in this installation
    When I open "Tasks"
    And I press "New Task"
    And I type "Read another repository" into "Name"
    And I choose "standard" in "Envelope"
    And I type '[{"git": {"repository": "<owner>/<other-repository>", "ref": "git:ref:main"}}]' into "Workspace (advanced JSON array)"
    And I press "Save version 1"
    And I press "Run"
    And I press "Run now"
    Then I see "The run request was rejected. The requested workspace repository is unavailable. Ask a Steward administrator to verify that it is allowed and accessible."
    And the message names "<owner>/<other-repository>"
    And I do not see 'Admission("'
    And the page "Runs" lists 0 new entries

  # F2. 0.3.16: a raw text that does not name the branch (N-13, N-03).
  @known-failing-0.3.16
  Scenario: A branch that does not exist is refused by name
    When I open "Tasks"
    And I press "New Task"
    And I type "Read a missing branch" into "Name"
    And I choose "standard" in "Envelope"
    And I choose "<owner>/<repository>" in "Workspace repository"
    And I type "no-such-branch" into "Branch or tag"
    And I press "Save version 1"
    And I press "Run"
    And I press "Run now"
    Then I see "The run request was rejected."
    And the message names "no-such-branch"
    And I do not see 'Admission("'
    And the page "Runs" lists 0 new entries

  # F3, first case. The Markdown gives no exact expected sentence.
  # 0.3.16: the text is wrapped and uses internal words (N-03).
  @known-failing-0.3.16
  Scenario: An Envelope that does not allow Git workspaces refuses the checkout
    Given I hold an Envelope from the template "basic"
    When I open "Runs"
    And I press "Run now"
    And I choose "basic" in "Envelope"
    And I tick "Check out this repository"
    And I press "Run now"
    Then I see "The run request was rejected."
    And the message names "Git"
    And the message says what to do next
    And I do not see 'Admission("'
    And the page "Runs" lists 0 new entries

  # F3, second case. The Envelope allows 1,000 generations.
  # 0.3.16: the text is wrapped (N-03).
  @known-failing-0.3.16
  Scenario: A history depth above the Envelope's limit is refused
    When I open "Runs"
    And I press "Run now"
    And I tick "Check out this repository"
    # 1,001 generations.
    And I type "1001" into "History depth (optional)"
    And I press "Run now"
    Then I see "The run request was rejected."
    And I see "Git history depth 1001 is more than the 1000 generations this Envelope allows."
    And I do not see 'Admission("'
    And the page "Runs" lists 0 new entries

  # Form checks on the way. The depth is counted in generations.
  Scenario Outline: A history depth that is not a positive whole number cannot be sent
    When I open "Runs"
    And I press "Run now"
    And I tick "Check out this repository"
    And I type "<depth>" into "History depth (optional)"
    Then I see "History depth must be a positive whole number."
    And "Run now" is disabled

    Examples:
      | depth |
      | 0     |
      | -1    |

  Scenario: The branch field cannot be left empty
    When I open "Tasks"
    And I press "New Task"
    And I choose "<owner>/<repository>" in "Workspace repository"
    And I leave "Branch or tag" empty
    Then the field "Branch or tag" shows "main"
