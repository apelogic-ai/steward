# Source: user journey J3. UI text exercised against the 0.3.24 release candidate.
# "I" is one person who is both a user and an administrator: I ask as a user,
# switch to the administrator area of the same UI, and decide my own request.
# Only the scenario about separation of roles uses a second account.
# Every scenario runs with the ticket system switched off.
Feature: J3 - User needs more than the template allows
  As a user
  I want an Envelope with one value above the template
  So that I can work once an administrator has seen exactly what I ask and approved it

  Background:
    # Approval must not depend on the ticket system.
    Given the optional integration "ticket system" is switched off
    And the optional integration "repository integration" is switched off
    And the optional integration "tool connections" is switched off
    And the template "timed" exists
    And the template "standard" exists
    And I am signed in as a user who is also an administrator

  # Steps 1 to 9 and "Done when".
  @smoke @needs-model
  Scenario: Approve - a request above the template is approved and used
    When I open "Envelopes"
    And I press "Request envelope"
    And I choose "timed" in "Template"
    Then the field "Runtime limit per month" shows "60"
    And I see in "Runtime limit per month":
      | min |
    # Keeps this Envelope distinct from active Envelopes left by earlier smoke runs.
    When I type "<run-runtime-minutes>" into "Runtime limit per month"
    And I type "20" into "Time-to-live limit"
    And I choose "min" in "Time-to-live limit"
    Then the field "Time-to-live limit" has a warning outline
    And I see "Above limit 15 min"
    And I see "Needs admin approval: 1 value above the template's limits"
    And I see "Time-to-live limit: 20 min (limit 15 min)"
    And I see "Submit for approval"
    # Expected sentence. 0.3.16 shows "... and need admin approval."
    And I see "1 value exceeds the template limits and needs admin approval."
    # The reason is required for a request above the template limits.
    When I type "The nightly build needs 20 min." into "05 Request note"
    And I press "Submit for approval"
    Then I see the status "pending"
    And I see "Pending administrator approval"
    And I see "Time-to-live limit: 20 min (limit 15 min)"
    And I see "Justification: The nightly build needs 20 min."
    When I choose "Admin workspace" in "Workspace view"
    And I open "Requests"
    Then I see in "Needs action":
      | timed |
    When I open "timed" in "Needs action"
    Then I see "Requested above the template limits"
    And I see in "Requester justification":
      | The nightly build needs 20 min. |
    And I see in "Requested vs template limits":
      | Time-to-live limit | 20 min | 15 min |
    And I see in "Workspace access":
      | Requested           | Template limit      |
      | No workspace access | No workspace access |
    When I type "Approved for the nightly build." into "Rationale"
    And I leave "Expires at (optional)" empty
    And I press "Approve exception"
    # UI text to confirm: the word shown for an approved request.
    Then I see the status "approved"
    # Gap: how the user learns of the decision (page only, or a notice).
    When I choose "User workspace" in "Workspace view"
    And I open "timed" in "Envelopes"
    Then I see the status "provisioned"
    And I see "Time-to-live limit 20 min"
    When I start a Hello world run with the Envelope from the template "timed"
    Then the run ends as "succeeded" within 2 min

  # Variant A. Gap: never reached; the Markdown gives no texts for the rejected state.
  # 0.3.16: BLOCKED at the decision (case E-08; no finding number).
  @known-failing-0.3.16
  Scenario: Reject - the request is rejected and the user can ask again
    When I open "Envelopes"
    And I press "Request envelope"
    And I choose "timed" in "Template"
    And I type "20" into "Time-to-live limit"
    And I choose "min" in "Time-to-live limit"
    And I type "The nightly build needs 20 min." into "05 Request note"
    And I press "Submit for approval"
    Then I see "Pending administrator approval"
    When I choose "Admin workspace" in "Workspace view"
    And I open "Requests"
    And I open "timed" in "Needs action"
    And I type "Not needed for this work." into "Rationale"
    And I press "Reject"
    And I choose "User workspace" in "Workspace view"
    And I open "timed" in "Envelopes"
    # UI text to confirm: the word shown for a rejected request, and where the rationale is shown.
    Then I see the status "rejected"
    And I see "Not needed for this work."
    And I do not see "provisioned"
    # No Envelope exists, so none can be chosen for a run.
    When I open "Runs"
    And I press "Run now"
    Then I do not see "timed" in "Envelope"
    # The user can ask again.
    When I open "Envelopes"
    And I press "Request envelope"
    And I choose "timed" in "Template"
    Then "Submit request" is enabled

  # After either decision the request leaves the queue and is listed as decided.
  # Gap: no case says where decided requests are listed, or gives the texts.
  # 0.3.16: BLOCKED at the decision (case E-08; no finding number).
  @known-failing-0.3.16
  Scenario Outline: A decided request leaves the queue and is listed as decided: <decision>
    When I open "Envelopes"
    And I press "Request envelope"
    And I choose "timed" in "Template"
    And I type "20" into "Time-to-live limit"
    And I choose "min" in "Time-to-live limit"
    And I type "The nightly build needs 20 min." into "05 Request note"
    And I press "Submit for approval"
    Then I see "Pending administrator approval"
    When I choose "Admin workspace" in "Workspace view"
    And I open "Requests"
    And I open "timed" in "Needs action"
    And I type "Decided after review." into "Rationale"
    And I press "<decision>"
    And I open "Requests"
    Then I do not see "timed" in "Needs action"
    # UI text to confirm: the name of the list of decided requests and its columns.
    And each decided request shows who asked, what was asked, who decided, when, the outcome and the rationale
    When I choose "User workspace" in "Workspace view"
    And I open "timed" in "Envelopes"
    Then I see the status "<status the user sees>"

    Examples:
      | decision          | status the user sees |
      | Approve exception | provisioned          |
      | Reject            | rejected             |

  # Separation of roles. UI text to confirm: what a person without the
  # administrator role is told. "a user" has no administrator role.
  # 0.3.16: not exercised (Gap: J9 F2).
  @two-accounts @nightly
  Scenario: A user without the administrator role cannot see the queue or decide
    Given a user is signed in on a separate account
    When I open "Envelopes"
    And I press "Request envelope"
    And I choose "timed" in "Template"
    And I type "20" into "Time-to-live limit"
    And I choose "min" in "Time-to-live limit"
    And I type "The nightly build needs 20 min." into "05 Request note"
    And I press "Submit for approval"
    Then I see "Pending administrator approval"
    And a user does not see "Admin workspace"
    When a user tries to open the administrator page "Requests"
    Then the message names "administrator"
    And a user does not see "Needs action"
    And a user does not see "Approve exception"
    And a user does not see "Reject"
    # The request is still waiting.
    When I open "timed" in "Envelopes"
    Then I see the status "pending"

  # Variant B.
  # 0.3.16: BLOCKED at the decision (case E-08; no finding number); a custom request
  # with workspace settings is refused (#405); the box can differ from what is sent (N-10).
  @needs-model @known-failing-0.3.16
  Scenario: Custom - a request without a template goes through the same approval
    When I open "Envelopes"
    And I press "Request envelope"
    And I choose "Custom envelope" in "Template"
    And I fill "Complete envelope JSON" with a complete Envelope description
    # The "Workspace access" boxes follow what was filled in.
    Then I see "These controls update workspace types and total size. Existing file, history, and submodule limits are preserved."
    When I press "Submit request"
    Then I see "Custom envelope"
    And I see the status "pending"
    And I see "This custom envelope request needs administrator approval."
    When I choose "Admin workspace" in "Workspace view"
    And I open "Requests"
    Then I see in "Needs action":
      | Custom envelope |
    When I open "Custom envelope" in "Needs action"
    And I type "Approved as asked." into "Rationale"
    And I leave "Expires at (optional)" empty
    And I press "Approve exception"
    Then I see the status "approved"
    When I choose "User workspace" in "Workspace view"
    And I open "Custom envelope" in "Envelopes"
    Then I see the status "provisioned"
    When I start a Hello world run with the Envelope from the template "Custom envelope"
    Then the run ends as "succeeded" within 2 min

  # Step 4 for the other template shapes.
  # 0.3.16: FAIL at step 4, the request is refused (#405).
  @known-failing-0.3.16
  Scenario Outline: A request above the template waits for approval whatever the template: <shape>
    Given the template "<template>" exists
    When I open "Envelopes"
    And I press "Request envelope"
    And I choose "<template>" in "Template"
    And I type "20" into "Time-to-live limit"
    And I choose "min" in "Time-to-live limit"
    And I press "Submit for approval"
    Then I see the status "pending"
    And I see "Pending administrator approval"

    Examples:
      | template | shape                   |
      | basic    | no runtime limit        |
      | standard | with workspace settings |

  # F2. 0.3.16: "Approvals are unavailable. Try again." and trying again does not help
  # (case E-08; no finding number).
  @known-failing-0.3.16
  Scenario: An administrator who cannot decide is told what to do next
    Given I have a request for the template "timed" with the status "pending"
    And the approval service is not available
    When I choose "Admin workspace" in "Workspace view"
    And I open "Requests"
    And I open "timed" in "Needs action"
    Then the message says what to do next

  # F3.
  Scenario: A custom request with a size that is not a number cannot be submitted
    When I open "Envelopes"
    And I press "Request envelope"
    And I choose "Custom envelope" in "Template"
    And I fill "Complete envelope JSON" with a complete Envelope description
    And I type "abc" into "Total size limit (optional)"
    Then I see "Enter a positive whole-number total size, or leave it blank."
    And "Submit request" is disabled
    When I leave "Total size limit (optional)" empty
    Then "Submit request" is enabled
