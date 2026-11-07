#!/usr/bin/env python3
import json
import pathlib
import sys


def main() -> int:
    if len(sys.argv) != 2:
        print(f"usage: {sys.argv[0]} STEWARD_VALUES_SCHEMA", file=sys.stderr)
        return 2

    path = pathlib.Path(sys.argv[1])
    schema = json.loads(path.read_text(encoding="utf-8"))
    properties = schema.get("properties")
    if not isinstance(properties, dict) or "global" in properties:
        print("Steward values schema has an unexpected properties contract", file=sys.stderr)
        return 1
    properties["global"] = {"type": "object"}
    path.write_text(json.dumps(schema, indent=2) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
