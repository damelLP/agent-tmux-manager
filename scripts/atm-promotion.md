# Promotion tools (Linux, Python 3.9+)

## Protected-file report

From the repository root:

```sh
python3 scripts/atm-protected-files.py                 # origin/main to HEAD
python3 scripts/atm-protected-files.py BEFORE AFTER    # exact merge endpoints
```

The report prints both resolved commit hashes, status and JSON-escaped path for
each protected change, and a count. Both changed and clean reports exit zero;
invalid revisions/rules or Git errors exit nonzero. No fetching, hooks or write
protection. It reports **net tree differences**, not intermediate reverted edits,
and never substitutes the merge base. Renames are D+A, including moves out of
protected directories. Mode changes and binary changes are included.

`scripts/protected-files.txt` holds repository-relative `fnmatchcase` patterns;
`*` spans `/`. Blank/comment lines are ignored. Rules are the union of the shipped
list and lists in both revisions, so narrowing/deleting the new list cannot hide
changes covered by the old one. A missing historical list is allowed. The tools,
list and their tests protect themselves through `scripts/*`.
