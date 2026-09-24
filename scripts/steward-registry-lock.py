#!/usr/bin/env python3
"""Mirror immutable Steward artifacts and write a verified deployment lock."""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path
from typing import Any


SHA256_RE = re.compile(r"^sha256:[0-9a-f]{64}$")
TARGET_PREFIX_RE = re.compile(
    r"^(?:localhost|[a-z0-9](?:[a-z0-9.-]*[a-z0-9])?)(?::[0-9]+)?"
    r"(?:/[a-z0-9]+(?:[._-][a-z0-9]+)*)*$"
)
PLATFORM_RE = re.compile(
    r"^(?P<os>[a-z0-9]+(?:[._-][a-z0-9]+)*)/"
    r"(?P<architecture>[a-z0-9]+(?:[._-][a-z0-9]+)*)"
    r"(?:/(?P<variant>[a-z0-9]+(?:[._-][a-z0-9]+)*))?$"
)


class LockError(Exception):
    """A safe, user-facing mirror failure."""


def run_docker(arguments: list[str]) -> bytes:
    try:
        result = subprocess.run(
            ["docker", *arguments],
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
    except FileNotFoundError as error:
        raise LockError("docker with the buildx imagetools plugin is required") from error
    except subprocess.CalledProcessError as error:
        detail = error.stderr.decode("utf-8", errors="replace").strip()
        if detail:
            raise LockError(f"docker command failed: {detail}") from error
        raise LockError("docker command failed") from error
    return result.stdout


def inspect_raw(reference: str) -> dict[str, Any]:
    raw = run_docker(["buildx", "imagetools", "inspect", "--raw", reference])
    try:
        value = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise LockError(f"registry returned invalid OCI JSON for {reference}") from error
    if not isinstance(value, dict):
        raise LockError(f"registry returned a non-object OCI manifest for {reference}")
    return value


def resolve_digest(reference: str) -> str:
    output = run_docker(["buildx", "imagetools", "inspect", reference]).decode(
        "utf-8", errors="replace"
    )
    matches = re.findall(r"(?m)^Digest:\s*(sha256:[0-9a-f]{64})\s*$", output)
    if len(matches) != 1:
        raise LockError(f"could not resolve exactly one registry digest for {reference}")
    return matches[0]


def validate_digest(value: Any, label: str) -> str:
    if not isinstance(value, str) or SHA256_RE.fullmatch(value) is None:
        raise LockError(f"{label} must be a lowercase sha256 digest")
    if value == "sha256:" + "0" * 64:
        raise LockError(f"{label} must not be the all-zero placeholder")
    return value


def split_reference(value: Any, label: str) -> tuple[str, str]:
    if not isinstance(value, str) or not value or any(character.isspace() for character in value):
        raise LockError(f"{label} must be a non-empty OCI reference")
    if "://" in value or "@" in value:
        raise LockError(f"{label} must be a tag reference without a scheme or digest")
    slash = value.rfind("/")
    colon = value.rfind(":")
    if colon <= slash or colon == len(value) - 1:
        raise LockError(f"{label} must include an explicit tag")
    repository = value[:colon]
    tag = value[colon + 1 :]
    if not re.fullmatch(r"[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}", tag):
        raise LockError(f"{label} contains an invalid tag")
    return repository, tag


def parse_platform(value: str | None) -> dict[str, str] | None:
    if value is None:
        return None
    match = PLATFORM_RE.fullmatch(value)
    if match is None:
        raise LockError("--platform must use os/architecture or os/architecture/variant")
    return {key: part for key, part in match.groupdict().items() if part is not None}


def descriptor_platform(descriptor: dict[str, Any]) -> dict[str, str] | None:
    platform = descriptor.get("platform")
    if not isinstance(platform, dict):
        return None
    operating_system = platform.get("os")
    architecture = platform.get("architecture")
    if not isinstance(operating_system, str) or not isinstance(architecture, str):
        return None
    result = {"os": operating_system, "architecture": architecture}
    variant = platform.get("variant")
    if isinstance(variant, str) and variant:
        result["variant"] = variant
    return result


def index_descriptors(manifest: dict[str, Any]) -> list[dict[str, Any]] | None:
    descriptors = manifest.get("manifests")
    if descriptors is None:
        return None
    if not isinstance(descriptors, list) or not descriptors:
        raise LockError("OCI index has no manifests")
    parsed: list[dict[str, Any]] = []
    for descriptor in descriptors:
        if not isinstance(descriptor, dict):
            raise LockError("OCI index contains a non-object descriptor")
        validate_digest(descriptor.get("digest"), "OCI descriptor digest")
        parsed.append(descriptor)
    return parsed


def descriptor_signature(descriptor: dict[str, Any]) -> str:
    selected = {
        key: descriptor[key]
        for key in ("digest", "mediaType", "size", "platform", "artifactType", "annotations")
        if key in descriptor
    }
    return json.dumps(selected, sort_keys=True, separators=(",", ":"))


def index_metadata(manifest: dict[str, Any]) -> str:
    selected = {
        key: manifest[key]
        for key in ("schemaVersion", "mediaType", "artifactType", "annotations", "subject")
        if key in manifest
    }
    return json.dumps(selected, sort_keys=True, separators=(",", ":"))


def platform_record(descriptor: dict[str, Any]) -> dict[str, str]:
    platform = descriptor_platform(descriptor)
    if platform is None:
        raise LockError("OCI image descriptor is missing an explicit platform")
    return {**platform, "digest": validate_digest(descriptor.get("digest"), "platform digest")}


def select_platform(
    descriptors: list[dict[str, Any]], requested: dict[str, str]
) -> dict[str, Any]:
    matches = [
        descriptor
        for descriptor in descriptors
        if descriptor_platform(descriptor) == requested
    ]
    if len(matches) != 1:
        display = "/".join(requested[key] for key in ("os", "architecture", "variant") if key in requested)
        raise LockError(f"source index does not contain exactly one {display} image")
    return matches[0]


def read_handoff(path: Path) -> dict[str, Any]:
    try:
        with path.open(encoding="utf-8") as stream:
            value = json.load(stream)
    except (OSError, json.JSONDecodeError) as error:
        raise LockError(f"could not read release handoff: {error}") from error
    if not isinstance(value, dict) or value.get("schemaVersion") != "steward.release-handoff/v1":
        raise LockError("release handoff must use steward.release-handoff/v1")
    if not isinstance(value.get("version"), str) or not value["version"]:
        raise LockError("release handoff version is required")
    if not isinstance(value.get("commit"), str) or re.fullmatch(r"[0-9a-f]{40}", value["commit"]) is None:
        raise LockError("release handoff commit must be a full lowercase Git SHA-1")
    return value


def artifact_entries(handoff: dict[str, Any]) -> list[tuple[str, str, dict[str, Any]]]:
    entries: list[tuple[str, str, dict[str, Any]]] = []
    for section in ("images", "referenceRuntimes"):
        values = handoff.get(section, {})
        if not isinstance(values, dict):
            raise LockError(f"release handoff {section} must be an object")
        for name in sorted(values):
            value = values[name]
            if not isinstance(name, str) or not name or not isinstance(value, dict):
                raise LockError(f"release handoff {section} entries must be named objects")
            entries.append((section, name, value))
    if not any(section == "images" for section, _, _ in entries):
        raise LockError("release handoff contains no Steward images")
    return entries


def mirror_artifact(
    section: str,
    name: str,
    value: dict[str, Any],
    target_prefix: str,
    requested_platform: dict[str, str] | None,
    target_tag_suffix: str,
) -> tuple[dict[str, Any], str, str]:
    source_repository, source_tag = split_reference(value.get("reference"), f"{section}.{name}.reference")
    source_digest = validate_digest(value.get("digest"), f"{section}.{name}.digest")
    source_exact = f"{source_repository}@{source_digest}"
    target_repository = f"{target_prefix}/{source_repository.rsplit('/', 1)[-1]}"
    target_tag = f"{source_tag}{target_tag_suffix}"
    if len(target_tag) > 128 or re.fullmatch(r"[A-Za-z0-9_][A-Za-z0-9_.-]*", target_tag) is None:
        raise LockError(f"target tag for {section}.{name} is invalid")
    target_reference = f"{target_repository}:{target_tag}"

    source_manifest = inspect_raw(source_exact)
    source_descriptors = index_descriptors(source_manifest)
    source_index_digest: str | None = None

    if requested_platform is None:
        if source_descriptors is None:
            raise LockError(
                f"{section}.{name} is a single manifest; rerun with --platform so the narrowing is explicit"
            )
        run_docker(["buildx", "imagetools", "create", "--tag", target_reference, source_exact])
        target_digest = resolve_digest(target_reference)
        target_manifest = inspect_raw(f"{target_repository}@{target_digest}")
        target_descriptors = index_descriptors(target_manifest)
        if target_descriptors is None:
            raise LockError(f"target {section}.{name} lost its OCI index")
        source_signatures = sorted(descriptor_signature(item) for item in source_descriptors)
        target_signatures = sorted(descriptor_signature(item) for item in target_descriptors)
        if (
            target_signatures != source_signatures
            or index_metadata(target_manifest) != index_metadata(source_manifest)
        ):
            raise LockError(f"target {section}.{name} does not preserve the source index descriptors")
        platforms = sorted(
            (platform_record(item) for item in source_descriptors if descriptor_platform(item) is not None),
            key=lambda item: (item["os"], item["architecture"], item.get("variant", ""), item["digest"]),
        )
        copy_mode = "index"
    else:
        if source_descriptors is None:
            selected_digest = source_digest
            selected_manifest = source_manifest
        else:
            source_index_digest = source_digest
            selected = select_platform(source_descriptors, requested_platform)
            selected_digest = validate_digest(selected.get("digest"), "selected platform digest")
            selected_manifest = inspect_raw(f"{source_repository}@{selected_digest}")
            if index_descriptors(selected_manifest) is not None:
                raise LockError(f"selected {section}.{name} platform still resolves to an OCI index")
        run_docker(
            [
                "buildx",
                "imagetools",
                "create",
                "--prefer-index=false",
                "--tag",
                target_reference,
                f"{source_repository}@{selected_digest}",
            ]
        )
        target_digest = resolve_digest(target_reference)
        target_manifest = inspect_raw(f"{target_repository}@{target_digest}")
        if index_descriptors(target_manifest) is not None:
            raise LockError(f"target {section}.{name} single-platform copy unexpectedly produced an index")
        if target_manifest != selected_manifest:
            raise LockError(f"target {section}.{name} single-platform manifest differs from its source")
        platforms = [{**requested_platform, "digest": selected_digest}]
        copy_mode = "single-platform"

    result: dict[str, Any] = {
        "copyMode": copy_mode,
        "platforms": platforms,
        "source": {"reference": f"{source_repository}:{source_tag}", "digest": source_digest},
        "target": {"reference": target_reference, "digest": target_digest},
    }
    if source_index_digest is not None:
        result["sourceIndexDigest"] = source_index_digest
    return result, target_repository, target_tag


def mirror(arguments: argparse.Namespace) -> None:
    handoff = read_handoff(arguments.handoff)
    target_prefix = arguments.target_prefix.rstrip("/")
    if TARGET_PREFIX_RE.fullmatch(target_prefix) is None:
        raise LockError("--target-prefix must be a lowercase registry host and optional repository path")
    requested_platform = parse_platform(arguments.platform)
    if arguments.target_tag_suffix and requested_platform is None:
        raise LockError("--target-tag-suffix is only valid with --platform")

    artifacts: dict[str, Any] = {}
    image_values: dict[str, Any] = {}
    execution_binding_images: dict[str, str] = {}
    image_repository: str | None = None
    target_references: set[str] = set()

    for section, name, value in artifact_entries(handoff):
        result, target_repository, target_tag = mirror_artifact(
            section,
            name,
            value,
            target_prefix,
            requested_platform,
            arguments.target_tag_suffix,
        )
        target_reference = result["target"]["reference"]
        if target_reference in target_references:
            raise LockError(f"multiple artifacts map to target tag {target_reference}")
        target_references.add(target_reference)
        artifacts[f"{section}.{name}"] = result
        if section == "images":
            if image_repository is None:
                image_repository = target_repository
            elif image_repository != target_repository:
                raise LockError("Steward component images must share one target repository")
            if name == "bridge":
                continue
            image_values[name] = {"tag": target_tag, "digest": result["target"]["digest"]}
        else:
            execution_binding_images[name] = (
                f"{target_repository}@{result['target']['digest']}"
            )

    if image_repository is None:
        raise LockError("release handoff contains no Steward image repository")
    chart_values: dict[str, Any] = {
        "images": {"repository": image_repository, **image_values}
    }
    bridge = artifacts.get("images.bridge")
    if bridge is not None:
        chart_values["connectionsBridge"] = {
            "image": f"{bridge['target']['reference'].rsplit(':', 1)[0]}@{bridge['target']['digest']}"
        }

    lock = {
        "schemaVersion": "steward.deployment-lock/v1",
        "release": {"version": handoff["version"], "commit": handoff["commit"]},
        "mode": "single-platform" if requested_platform is not None else "index",
        "requestedPlatform": arguments.platform,
        "artifacts": artifacts,
        "chartValues": chart_values,
        "executionBindingImages": execution_binding_images,
    }
    serialized = json.dumps(lock, indent=2, sort_keys=True) + "\n"
    try:
        arguments.output.write_text(serialized, encoding="utf-8")
    except OSError as error:
        raise LockError(f"could not write deployment lock: {error}") from error


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(
        description="Mirror exact Steward release artifacts using Docker's configured registry authentication."
    )
    subcommands = result.add_subparsers(dest="command", required=True)
    command = subcommands.add_parser("mirror", help="copy and verify artifacts, then write a lock")
    command.add_argument("--handoff", type=Path, required=True)
    command.add_argument("--target-prefix", required=True)
    command.add_argument("--output", type=Path, required=True)
    command.add_argument(
        "--platform",
        help="explicitly narrow every artifact to one os/architecture[/variant] platform",
    )
    command.add_argument(
        "--target-tag-suffix",
        default="",
        help="suffix target tags in explicit single-platform mode",
    )
    command.set_defaults(function=mirror)
    return result


def main() -> int:
    arguments = parser().parse_args()
    try:
        arguments.function(arguments)
    except LockError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
