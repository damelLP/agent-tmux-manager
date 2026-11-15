# Task CLI method answers

Captured: atmd b3f75b26, 2026-11-15, the private `DaemonServer` method-fixture test with committed synthetic task state (no harness). The seed clock is `2026-01-15T12:00:00Z`; requests and setups come from the daemon's `tests/fixtures/tasks` contracts. `method_answers_match_the_recorded_contract` checks these recorded answers against the real methods, including exact refusal text.

`cases.json` holds successful method results, hand-written CLI arguments/complete parameter objects, and intended text goldens. `refusals.json` records one error for each method. Scripted CLI tests replay these errors to verify passthrough independently of which otherwise-valid CLI arguments elicited the call: notably the CLI rejects `list --limit 0` itself, while the daemon's captured limit refusal is still a valid error response to display.

Forward-compatibility, malformed-response, control-character, pagination, unconfirmed-notification, and optional-argument variants in the tests are synthetic and labeled at their assertions; they are not claimed to be harness captures.
