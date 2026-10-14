# Source: user journey J7. Original product findings were recorded against version 0.3.16.
# "The log" is the tab that is shown first when "Agent execution" is opened ("stdout").
Feature: J7 - User watches and stops a run
  As a user who started a run that takes minutes
  I want to see what the agent is doing and stop it
  So that I see what was captured and what it cost

  Background:
    Given the optional integration "ticket system" is switched off
    And the optional integration "repository integration" is switched off
    And the optional integration "tool connections" is switched off
    And the installation keeps execution logs
    And I am signed in as a user with the role "member"
    And I hold an Envelope from the template "standard"

  # Steps 1 to 7 and "Done when".
  # 0.3.16: FAIL at step 3 (N-07) and FAIL at step 5 (N-05).
  # Specification only until cancellation coverage is executable; no dedicated issue replaces the release label.
  @needs-model
  Scenario: Watch a run, cancel it and see what was captured and spent
    When I open "Runs"
    And I press "Run now"
    # Select the Background Envelope explicitly; the Run form otherwise chooses the newest active one.
    And I choose "standard" in "Envelope"
    # The long loop exercises live logs; writing under out/ lets a successful inline run publish output.
    And I type "Print one line every 10 s for 2 min, then write the complete transcript to out/result.txt." into "Prompt"
    And I tick "Capture execution log"
    And I press "Run now"
    Then I see the status "Running"
    And the elapsed time keeps counting
    When I open "Agent execution"
    Then I see "stdout"
    And I see "stderr"
    And I see "Open full page"
    And I see "Live log · polling for new output…"
    And the log grows by new lines within 30 s while the run has the status "Running"
    When I open "More run actions"
    And I press "Cancel run"
    Then I see "Cancel this run?"
    And I see "The agent will stop and its temporary credentials will be revoked. This cannot be undone."
    And I see "Keep running"
    # The button in the dialog.
    When I press "Cancel run"
    Then the run ends as "cancelled" within 1 min
    When I open "Agent execution"
    # If no final log was saved, the page also says: "The run ended before the
    # full log was saved. This is the output captured while it ran."
    Then the log holds every line printed before the run ended
    And I do not see "Live log · polling for new output…"
    # Gap: the spend of a cancelled run was never reached.
    And the spend of the run is above 0.00 USD
    And every amount of money on the page has 2 decimals and the unit "USD"
    When I open "Envelopes"
    Then the Envelope from the template "standard" shows the same spend as used out of its monthly limit

  # Variant. Passes at its end on 0.3.16.
  @extended @needs-model
  Scenario: Variant - a run left to finish has a complete log
    When I open "Runs"
    And I press "Run now"
    # Select the Background Envelope explicitly; the Run form otherwise chooses the newest active one.
    And I choose "standard" in "Envelope"
    # The long loop exercises complete logs; writing under out/ lets a successful inline run publish output.
    And I type "Print one line every 10 s for 2 min, then write the complete transcript to out/result.txt." into "Prompt"
    And I tick "Capture execution log"
    And I press "Run now"
    Then the run ends as "succeeded" within 4 min
    When I open "Agent execution"
    Then the log holds every line printed before the run ended
    And I do not see "Live log · polling for new output…"

  # F1. PASS on 0.3.16.
  @extended @needs-model
  Scenario: A run started without log capture says so
    When I open "Runs"
    And I press "Run now"
    # Select the Background Envelope explicitly; the Run form otherwise chooses the newest active one.
    And I choose "standard" in "Envelope"
    And I untick "Capture execution log"
    And I press "Run now"
    Then the run ends as "succeeded" within 2 min
    When I open "Agent execution"
    Then I see "No execution log was captured for this run."
    And I see "Enable Capture execution log when starting a run to retain stdout and stderr. Captured output may contain sensitive data."

  # F2. The Markdown does not say when a run rightly cannot be cancelled.
  # 0.3.16: the owner of the run sees "You don't have permission to cancel this run." (N-05).
  # Specification only: the historical release label is not a dedicated issue identity.
  @needs-model
  Scenario: A run that cannot be cancelled says why and what to do
    Given I am on the page of a run that cannot be cancelled
    When I open "More run actions"
    And I press "Cancel run"
    And I press "Cancel run"
    Then the message says what to do next

  # F3. The run failed before the agent started.
  # 0.3.16: only "Agent execution — Failed" and "The captured stdout log is empty." (N-02, N-04).
  @known-failing-469
  Scenario: A run that fails before the agent starts gives a reason
    Given I have a run that ended as "failed" without a captured execution log
    When I open "Runs"
    And I open the run that ended as "failed"
    Then I see the reason the run failed in plain words
