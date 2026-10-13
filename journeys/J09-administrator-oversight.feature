# Source: user journey J9. Original product findings were recorded against version 0.3.16.
# "I" is the administrator. "another user" owns the runs and the requests.
@two-accounts
Feature: J9 - Administrator oversight
  As an administrator who is accountable for what runs and what it costs
  I want to see every user's runs, logs, spend and requests
  So that I can answer who ran what, what it printed, what it cost and what is waiting

  Background:
    Given the optional integration "ticket system" is switched off
    And the optional integration "repository integration" is switched off
    And the optional integration "tool connections" is switched off
    And another user has a run that ended as "succeeded" with a captured execution log
    And another user has a run that ended as "failed" with a captured execution log
    And another user has a request for the template "timed" with the status "pending"
    And another user has a request for the template "standard" with the status "approved"
    And I am signed in as a user with the role "administrator"
    And I choose "Admin workspace" in "Workspace view"

  # Steps 1 to 6 and "Done when".
  # 0.3.16: FAIL at step 1 (N-08, N-06); step 3 gives no reason (N-02, N-04).
  @known-failing-407
  Scenario: See every user's runs, a log, the spend and the requests
    When I open "Runs"
    Then I see "Every governed run across all users."
    And each run in the list shows its owner, its status and its spend
    And every amount of money on the page has 2 decimals and the unit "USD"
    When I open the run that ended as "succeeded"
    # A run that called a model never shows "0.00 USD".
    Then the spend of the run is above 0.00 USD
    When I open "Agent execution"
    And I open "stdout"
    # The same content the owner sees. Reading does not change the log.
    Then the log holds every line printed before the run ended
    And I see "Sensitivity notice"
    When I open "Runs"
    And I open the run that ended as "failed"
    Then I see the reason the run failed in plain words
    When I open "Requests"
    Then I see in "Needs action":
      | timed |
    When I open "timed" in "Needs action"
    Then I see "Requested above the template limits"
    And I see "Requester justification"
    And I see "Requested vs template limits"
    And I see "Workspace access"
    # Gap: no case says where decided requests are listed.
    When I open "Requests"
    Then each decided request shows who asked, what was asked, who decided, when, the outcome and the rationale

  # F1.
  Scenario: A run started without log capture says so to the administrator
    Given another user has a run that ended as "cancelled" without a captured execution log
    When I open "Runs"
    And I open the run that ended as "cancelled"
    And I open "Agent execution"
    Then I see "No execution log was captured for this run."

  # F2. Gap: no existing case; the Markdown gives no exact text.
  Scenario: A person without the administrator role cannot open an administrator page
    Given a user is signed in on a separate account
    When a user tries to open the administrator page "Templates"
    Then the message names "administrator"
