"""widgets: a tiny inventory CLI."""
import argparse
import sys

ITEMS = [
    {"name": "gear", "size": 12, "updated": "2026-11-01"},
    {"name": "bolt", "size": 3, "updated": "2026-10-30"},
]


def main(argv=None):
    parser = argparse.ArgumentParser(prog="widgets")
    sub = parser.add_subparsers(dest="cmd", required=True)
    lst = sub.add_parser("list", help="list widgets, sorted by name")
    args = parser.parse_args(argv)
    if args.cmd == "list":
        for item in sorted(ITEMS, key=lambda i: i["name"]):
            print(f"{item['name']:<8} {item['size']:>4}  {item['updated']}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
