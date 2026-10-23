#!/usr/bin/env python3
"""Turn the pinned agent-sandbox release manifest into a Helm dependency chart."""

from __future__ import annotations

import re
import sys
from pathlib import Path


def fail(message: str) -> None:
    raise SystemExit(message)


def main() -> None:
    if len(sys.argv) != 3:
        fail("usage: prepare-agent-sandbox-chart.py MANIFEST CHART")

    manifest = Path(sys.argv[1])
    chart = Path(sys.argv[2])
    documents = re.split(r"(?m)^---[ \t]*$", manifest.read_text())
    crd: str | None = None
    templates: list[str] = []

    for document in documents:
        document = document.strip()
        if not document:
            continue
        kind_match = re.search(r"(?m)^kind:[ \t]*([^ \t\r\n]+)", document)
        if kind_match is None:
            fail("agent-sandbox manifest contains a document without kind")
        kind = kind_match.group(1)
        if kind == "Namespace":
            continue
        if kind == "CustomResourceDefinition":
            if crd is not None:
                fail("agent-sandbox manifest contains multiple CRDs")
            crd = document
            continue

        document = document.replace(
            "image: registry.k8s.io/agent-sandbox/agent-sandbox-controller:v0.5.0",
            'image: "{{ .Values.image.repository }}:{{ .Values.image.tag }}@{{ .Values.image.digest }}"\n'
            "        imagePullPolicy: {{ .Values.image.pullPolicy }}",
        )
        document = document.replace(
            "agent-sandbox-webhook-service",
            '{{ include "agent-sandbox.webhookServiceName" . }}',
        )
        document = document.replace(
            "agent-sandbox-controller",
            '{{ include "agent-sandbox.fullname" . }}',
        )
        document = document.replace(
            "agent-sandbox-system", "{{ .Values.targetNamespace }}"
        )
        templates.append(document)

    if crd is None:
        fail("agent-sandbox manifest contains no CRD")
    if not templates:
        fail("agent-sandbox manifest contains no workload resources")

    crd_directory = chart / "crds"
    template_directory = chart / "templates"
    crd_directory.mkdir(parents=True, exist_ok=True)
    template_directory.mkdir(parents=True, exist_ok=True)
    (crd_directory / "sandboxes.agents.x-k8s.io.yaml").write_text(
        "---\n" + crd + "\n"
    )
    (template_directory / "upstream.yaml").write_text(
        "---\n" + "\n---\n".join(templates) + "\n"
    )


if __name__ == "__main__":
    main()
