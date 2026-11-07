#!/usr/bin/env python3
import os
import pathlib
import re
import sys


COMPONENTS = (
    ("apiserver", "apiserver"),
    ("controller", "controller"),
    ("mint", "mint"),
    ("web", "web"),
    ("triggerReceiver", "trigger-receiver"),
)


def main() -> int:
    if len(sys.argv) != 8:
        print(
            f"usage: {sys.argv[0]} VALUES VERSION APISERVER CONTROLLER MINT WEB TRIGGER_RECEIVER",
            file=sys.stderr,
        )
        return 2

    path = pathlib.Path(sys.argv[1])
    version = sys.argv[2]
    digests = sys.argv[3:]
    repository = os.environ.get("STEWARD_BUNDLE_RELEASE_IMAGE_REPOSITORY", "")
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?", version):
        print(f"invalid Steward bundle release version: {version}", file=sys.stderr)
        return 2
    for digest in digests:
        if not re.fullmatch(r"sha256:[0-9a-f]{64}", digest):
            print(f"invalid Steward bundle release digest: {digest}", file=sys.stderr)
            return 2
    if repository and (
        len(repository) > 255
        or not re.fullmatch(
            r"[a-z0-9][a-z0-9.-]*(?::[0-9]{1,5})?/[a-z0-9][a-z0-9._/-]*",
            repository,
        )
    ):
        print(f"invalid Steward bundle image repository: {repository}", file=sys.stderr)
        return 2

    replacements = {
        component: (f"{version}-{suffix}", digest)
        for (component, suffix), digest in zip(COMPONENTS, digests, strict=True)
    }
    counts = {(component, field): 0 for component, _ in COMPONENTS for field in ("tag", "digest")}
    output: list[str] = []
    in_images = False
    component = ""
    repository_count = 0
    for line in path.read_text(encoding="utf-8").splitlines(keepends=True):
        if line == "  images:\n":
            in_images = True
            output.append(line)
            continue
        if in_images and repository and line.startswith("    repository:"):
            line = f"    repository: {repository}\n"
            repository_count += 1
        if in_images and line.startswith("  ") and not line.startswith("    ") and line.strip():
            in_images = False
            component = ""
        if in_images and line.startswith("    ") and not line.startswith("      "):
            candidate = line.strip().removesuffix(":")
            component = candidate if candidate in replacements else ""
        if component and line.startswith("      tag:"):
            line = f"      tag: {replacements[component][0]}\n"
            counts[(component, "tag")] += 1
        elif component and line.startswith("      digest:"):
            line = f"      digest: {replacements[component][1]}\n"
            counts[(component, "digest")] += 1
        output.append(line)

    missing = [f"{component}.{field}" for (component, field), count in counts.items() if count != 1]
    if repository and repository_count != 1:
        missing.append("repository")
    if missing:
        print(
            "Steward bundle values have an unexpected image contract: " + ", ".join(missing),
            file=sys.stderr,
        )
        return 1
    path.write_text("".join(output), encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
