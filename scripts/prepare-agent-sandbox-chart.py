#!/usr/bin/env python3
"""Prepare the pinned upstream chart for a digest-bearing image tag."""

from __future__ import annotations

import sys
from pathlib import Path


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit("usage: prepare-agent-sandbox-chart.py UPSTREAM_CHART")

    helpers = Path(sys.argv[1]) / "templates" / "_helpers.tpl"
    original = helpers.read_text()
    label = "app.kubernetes.io/version: {{ .Values.image.tag | quote }}"
    replacement = (
        'app.kubernetes.io/version: '
        '{{ first (splitList "@" .Values.image.tag) | quote }}'
    )
    if original.count(label) != 1:
        raise SystemExit("agent-sandbox image version label template changed upstream")
    helpers.write_text(original.replace(label, replacement))


if __name__ == "__main__":
    main()
