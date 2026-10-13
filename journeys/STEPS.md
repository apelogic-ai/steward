# Step vocabulary

Every sentence used in the ten `.feature` files follows one of the **63 patterns** below.
A new scenario should be written with these patterns. Add a pattern only when a person does or sees something that no pattern here can say.

## How to read a pattern

- `{string}` is a text in quotes. Use "double quotes"; use 'single quotes' when the text itself holds a double quote.
- `{int}` is a whole number. Its unit is written in the sentence (`min`, `s`, `entries`).
- `{actor}` is who acts: `I` (the journey's persona), `an administrator`, `a user`, `another user`, `the operator`.
  The verb follows the actor: "I see", "an administrator sees". In a pattern this is written `see(s)`.
- `a/b` means one of the two words, for example `off/on`.
- A pattern that ends with `:` is followed by a table.
- A sentence without an actor ("the field ... shows ...", "the message ...") is about the screen of the person who acted last.
- "Files" is the number of feature files (of 10) that use the pattern. "Uses" counts every use; a row of an Examples table counts once.

## Setup (5)

What must be true before the person starts.

| Pattern | Parameters | Meaning | Files | Uses | Used in |
|---|---|---|---|---|---|
| `the optional integration {string} is switched off/on` | name of the integration: "ticket system", "repository integration", "tool connections" | The integration is disabled (or enabled) for the whole scenario. | 10 | 176 | J1, J2, J3, J4, J5, J6, J7, J8, J9, J10 |
| `the installation's configuration has the coding agent {string} with the model {string}` | agent placeholder; model placeholder | The agent is configured and works with that model. | 3 | 20 | J1, J2, J8 |
| `the repository {string} is allowed/unavailable in this installation` | repository placeholder | The installation allows (or does not allow) this repository. | 1 | 11 | J4 |
| `the installation keeps execution logs` | none | Captured logs are stored. | 1 | 5 | J7 |
| `{actor} has/have a run that ended as {string} with/without a captured execution log` | who; how the run ended: "succeeded", "failed", "cancelled" | A finished run of that person exists. | 3 | 9 | J4, J7, J9 |

## Sign-in (3)

Who is at the screen.

| Pattern | Parameters | Meaning | Files | Uses | Used in |
|---|---|---|---|---|---|
| `I am signed in as a user with the role {string}` | role: "member", "administrator" | One account with that role. "member" stands for the role a template is meant for. | 7 | 41 | J2, J4, J5, J6, J7, J8, J9 |
| `I am signed in as a user who is also an administrator` | none | One account that holds the member role and the administrator role. The default for journeys that need both. | 2 | 17 | J1, J3 |
| `{actor} is signed in on a separate account` | who: "a user" (no administrator role), "an administrator" | A second person at a second browser. | 3 | 10 | J3, J9, J10 |

## Any screen (23)

Actions and sights that are the same on every screen.

| Pattern | Parameters | Meaning | Files | Uses | Used in |
|---|---|---|---|---|---|
| `{actor} open(s) {string}` | who; text of a menu entry, tab, menu or collapsed section | Goes to that place. | 10 | 95 | J1, J2, J3, J4, J5, J6, J7, J8, J9, J10 |
| `{actor} open(s) {string} in {string}` | who; text that identifies one entry; name of the list or tab | Opens one entry of a list, for example a template, an Envelope, a request or a Task. | 6 | 21 | J1, J3, J6, J8, J9, J10 |
| `{actor} press(es) {string}` | who; text of a button or link | Clicks it once. | 9 | 160 | J1, J2, J3, J4, J5, J6, J7, J8, J10 |
| `{actor} type(s) {string} into {string}` | who; what is typed, exactly; label of the field or section | Replaces what the field holds. An empty text clears the field. | 8 | 72 | J1, J2, J3, J4, J5, J6, J7, J8 |
| `{actor} choose(s) {string} in {string}` | who; text of the entry; label of the drop-down | Picks one entry. An Envelope is named by its template. | 9 | 104 | J1, J2, J3, J4, J5, J6, J8, J9, J10 |
| `{actor} tick(s) {string}` | who; label of the checkbox | Makes sure the box is ticked. | 5 | 17 | J1, J2, J4, J5, J7 |
| `{actor} untick(s) {string}` | who; label of the checkbox | Makes sure the box is not ticked. | 2 | 3 | J1, J7 |
| `{actor} leave(s) {string} empty` | who; label of the field | Makes sure the field is empty. | 3 | 6 | J1, J3, J4 |
| `{actor} remove(s) {string} from {string}` | who; text of the item; label of the field | Removes one item from a field that holds several. | 1 | 1 | J1 |
| `{actor} see(s) {string}` | who; exact UI text | The text is on the page. | 10 | 106 | J1, J2, J3, J4, J5, J6, J7, J8, J9, J10 |
| `{actor} do(es) not see {string}` | who; text | The text is nowhere on the page. | 9 | 32 | J1, J2, J3, J4, J5, J6, J7, J8, J10 |
| `{actor} see(s) in {string}:` | who; heading, label, tab or card that names the area; a table | Each table row is found in that area, its cells together. An empty cell is not checked. | 8 | 23 | J1, J2, J3, J4, J5, J6, J9, J10 |
| `{actor} do(es) not see {string} in {string}` | who; text; name of the area, list or drop-down | The text is not in that area. | 3 | 6 | J3, J8, J10 |
| `{actor} see(s) the status {string}` | who; status word | The status of the Envelope, request or run on the page. | 4 | 16 | J2, J3, J7, J10 |
| `the field {string} shows {string}` | label; text | The field holds or selects that text. | 6 | 15 | J1, J2, J3, J4, J8, J10 |
| `the field {string} has a warning outline` | label | The field is marked as needing attention. | 1 | 2 | J3 |
| `{string} is disabled` | text of a button, field or entry | It is greyed out and cannot be used. | 5 | 10 | J1, J2, J3, J4, J8 |
| `{string} is enabled` | text of a button, field or entry | It can be used. | 3 | 5 | J2, J3, J8 |
| `the drop-down {string} offers {string}` | label; text | One entry of the list contains the text. | 5 | 13 | J1, J2, J4, J8, J10 |
| `the page {string} lists {int} new entry/entries` | menu name of the list; count of entries | Compared with the start of the scenario. 0 means nothing was created. | 7 | 26 | J1, J2, J3, J4, J5, J6, J8 |
| `every amount of money on the page has 2 decimals and the unit "USD"` | none | For example "0.02 USD". | 3 | 3 | J2, J7, J9 |
| `the message names {string}` | text | The message shown after the last action contains that field, name or limit. | 5 | 9 | J1, J3, J4, J6, J9 |
| `the message says what to do next` | none | The message shown after the last action ends with an action the person can take. No exact text exists yet. | 4 | 12 | J3, J4, J6, J7 |

## Templates (3)

| Pattern | Parameters | Meaning | Files | Uses | Used in |
|---|---|---|---|---|---|
| `no template exists` | none | No template whose ID starts with this run's unique prefix exists; the runner records that prefix's starting count. | 1 | 6 | J1 |
| `the template {string} exists` | template placeholder | See the placeholder list in README.md for what each template allows. | 3 | 33 | J2, J3, J10 |
| `{actor} type(s) a value the installation does not accept into {string}` | who; label of the field | A value that makes saving fail. The Markdown does not say which. | 1 | 1 | J1 |

## Envelope requests (5)

| Pattern | Parameters | Meaning | Files | Uses | Used in |
|---|---|---|---|---|---|
| `I hold no Envelope` | none | The person has never had an Envelope. | 1 | 1 | J2 |
| `{actor} hold(s) an Envelope from the template {string}` | who; template placeholder | A provisioned Envelope with the limits of that template. | 7 | 56 | J2, J4, J5, J6, J7, J8, J10 |
| `{actor} has/have a request for the template {string} with the status {string}` | who; template placeholder; "pending" or "approved" | A request above the template limits exists in that state. | 2 | 7 | J3, J9 |
| `{actor} fill(s) {string} with a complete Envelope description` | who; label of the field | A complete custom request. The Markdown does not give its content. | 1 | 2 | J3 |
| `the approval service is not available` | none | The administrator cannot record a decision. The Markdown does not say how this comes about. | 1 | 1 | J3 |

## Tasks and runs (12)

| Pattern | Parameters | Meaning | Files | Uses | Used in |
|---|---|---|---|---|---|
| `{actor} start(s) a Hello world run with the Envelope from the template {string}` | who; template placeholder | Short for J2 steps 6 and 7: Runs, Run now, that Envelope, "Inline package", the Hello world prompt, "Capture execution log" ticked, Run now. | 3 | 4 | J2, J3, J10 |
| `the run ends as {string} within {int} min` | status word; minutes | Waits for the run that was just started. | 8 | 16 | J2, J3, J4, J5, J6, J7, J8, J10 |
| `the run output contains {string}` | text | The output shown on the Run page contains the text. | 3 | 3 | J2, J4, J5 |
| `the spend of the run is above 0.00 USD` | none | "Spend" on the Run page. | 3 | 3 | J2, J7, J9 |
| `the Envelope from the template {string} shows the same spend as used out of its monthly limit` | template placeholder | For example "0.02 USD / 10.00 USD" in the Envelopes list. | 2 | 2 | J2, J7 |
| `the run stops with a message about its limit per run` | none | The run does not go on past its limit per run. | 1 | 1 | J2 |
| `{string} shows a commit of 40 hex characters that is the head of the branch {string}` | label; branch name | Compared with the branch on the repository host. | 1 | 3 | J4 |
| `{actor} state(s) requirements that are all below/at the Envelope's limits` | who | Fills "Requirements (optional JSON)" completely, every value below (or equal to) the limit of the chosen Envelope. | 1 | 2 | J6 |
| `{actor} state(s) a requirement above the Envelope's limit: {string}` | who; the one need that is above the limit, in plain words with units | As above, with one value above the limit. | 1 | 8 | J6 |
| `{actor} has/have a saved Task named {string} with the coding agent {string} and the model {string}` | who; Task name; agent placeholder; model placeholder | A Task saved earlier. | 2 | 5 | J8, J10 |
| `{actor} open(s) the run that ended as {string}` | who; status word | Opens the Run page of the run named in the scenario's Given steps. | 3 | 5 | J4, J7, J9 |
| `I am on the page of a run that cannot be cancelled` | none | The Markdown does not say when a run rightly cannot be cancelled. | 1 | 1 | J7 |

## Logs (3)

| Pattern | Parameters | Meaning | Files | Uses | Used in |
|---|---|---|---|---|---|
| `the elapsed time keeps counting` | none | The time on the Run page goes up while the run is running. | 1 | 1 | J7 |
| `the log grows by new lines within {int} s while the run has the status {string}` | seconds; status word | The log tab that is shown first. | 1 | 1 | J7 |
| `the log holds every line printed before the run ended` | none | Nothing is missing up to the last line, for a finished or a cancelled run, for the owner and for an administrator. | 2 | 3 | J7, J9 |

## Administration (4)

| Pattern | Parameters | Meaning | Files | Uses | Used in |
|---|---|---|---|---|---|
| `each run in the list shows its owner, its status and its spend` | none | The administrator's Runs list. | 1 | 1 | J9 |
| `{actor} see(s) the reason the run failed in plain words` | who | No exact text exists yet. | 2 | 2 | J7, J9 |
| `each decided request shows who asked, what was asked, who decided, when, the outcome and the rationale` | none | No exact text or place exists yet. | 2 | 3 | J3, J9 |
| `{actor} try/tries to open the administrator page {string}` | who; menu name of the page | By any route the person has. | 2 | 2 | J3, J9 |

## Configuration (5)

Done by the operator, outside the web UI.

| Pattern | Parameters | Meaning | Files | Uses | Used in |
|---|---|---|---|---|---|
| `I upgrade the installation to the new version` | none | - | 1 | 1 | J10 |
| `every service is running with 0 restarts and 0 error lines` | none | Checked after the change has rolled out. | 1 | 2 | J10 |
| `I add the model {string} to the installation's configuration without a new product build` | model placeholder | - | 1 | 2 | J10 |
| `I apply a configuration that is not valid for this version` | none | - | 1 | 1 | J10 |
| `the services refuse to start with a message that names the setting` | none | - | 1 | 1 | J10 |

## Totals

- Patterns: **63** (Setup 5, Sign-in 3, Any screen 23, Templates 3, Envelope requests 5, Tasks and runs 12, Logs 3, Administration 4, Configuration 5).
- Used in 2 or more files: 41. Used in 1 file only: 22.
- The patterns whose meaning says "no exact text exists yet" or "the Markdown does not say" are placeholders. Each should be replaced by `{actor} see(s) {string}` with the real UI text, or by a concrete action, once that text is confirmed. That would lower the count.
- One piece of supporting code is needed besides the sentences: the `{actor}` parameter (a list of the five names above).
