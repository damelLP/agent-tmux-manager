# Plan: `--json` for `widgets list`

Survey: widgets.py has one subcommand, `list`, printing a fixed-width table
from ITEMS sorted by name (task 1). README.md documents it (task 2). No tests
exist yet.

Plan:
1. Add `--json` to the `list` subparser (store_true).
2. With `--json`, print `json.dumps(item)` per item: JSON Lines, keys `name`,
   `size`, `updated`, in that order. Without it, print the table as today.
3. Add test_widgets.py with two tests: the table output is unchanged, and each
   `--json` line parses and has exactly the three keys.
4. Run `python3 -m pytest -q`, or `python3 test_widgets.py` if pytest is missing.

No other files change. Questions: none.
