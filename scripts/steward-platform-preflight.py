#!/usr/bin/env python3
"""Generate and validate deterministic Steward deployment inputs."""

from __future__ import annotations

import argparse
import hashlib
import ipaddress
import json
import pathlib
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from typing import Any


CONTRACT = "steward.platform-input/v1"
RESULT_CONTRACT = "steward.platform-preflight-result/v1"
SHA256 = re.compile(r"^sha256:[0-9a-f]{64}$")
DNS_NAME = re.compile(r"^(?=.{1,253}$)(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$")
K8S_NAME = re.compile(r"^[a-z0-9](?:[a-z0-9.-]{0,251}[a-z0-9])?$")
REQUIRED_NAMESPACES = (
    "steward",
    "runtime",
    "providers",
    "gateway",
    "arc",
    "databaseTls",
    "mcpGateway",
    "litellm",
    "identityExchange",
    "openshell",
    "dns",
)
IMAGE_COMPONENTS = ("apiserver", "controller", "mint", "web", "bridge")


class ValidationError(Exception):
    pass


def install_cleanup_signal_handlers() -> dict[signal.Signals, Any]:
    def interrupt(signal_number: int, _frame: Any) -> None:
        raise ValidationError(
            f"network smoke interrupted by {signal.Signals(signal_number).name}; cleanup was attempted"
        )

    previous = {
        signal_number: signal.getsignal(signal_number)
        for signal_number in (signal.SIGINT, signal.SIGTERM)
    }
    for signal_number in previous:
        signal.signal(signal_number, interrupt)
    return previous


def restore_signal_handlers(previous: dict[signal.Signals, Any]) -> None:
    for signal_number, handler in previous.items():
        signal.signal(signal_number, handler)


def canonical(value: Any) -> str:
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False) + "\n"


def read_json(path: pathlib.Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ValidationError(f"cannot read JSON input {path}: {error}") from error
    if not isinstance(value, dict):
        raise ValidationError("input root must be an object")
    return value


def require_object(parent: dict[str, Any], key: str, path: str) -> dict[str, Any]:
    value = parent.get(key)
    if not isinstance(value, dict):
        raise ValidationError(f"{path}.{key} must be an object")
    return value


def require_string(parent: dict[str, Any], key: str, path: str) -> str:
    value = parent.get(key)
    if not isinstance(value, str) or not value.strip():
        raise ValidationError(f"{path}.{key} must be a non-empty string")
    return value


def require_exact_keys(parent: dict[str, Any], expected: set[str], path: str) -> None:
    unexpected = sorted(set(parent) - expected)
    if unexpected:
        raise ValidationError(f"{path} contains unsupported fields: {unexpected}")


def validate_name(value: str, path: str) -> None:
    if not K8S_NAME.fullmatch(value):
        raise ValidationError(f"{path} is not a valid Kubernetes name: {value}")


def validate_digest(value: str, path: str) -> None:
    if not SHA256.fullmatch(value):
        raise ValidationError(f"{path} must be an immutable sha256 digest")
    if value == "sha256:" + ("0" * 64):
        raise ValidationError(f"{path} is an all-zero placeholder digest")


def validate_required_cidrs(parent: dict[str, Any], key: str, path: str) -> list[str]:
    cidrs = parent.get(key)
    if not isinstance(cidrs, list) or not cidrs:
        raise ValidationError(f"{path}.{key} must be a non-empty CIDR array")
    seen: set[str] = set()
    for index, value in enumerate(cidrs):
        if not isinstance(value, str):
            raise ValidationError(f"{path}.{key}[{index}] must be a CIDR string")
        try:
            network = ipaddress.ip_network(value, strict=True)
        except ValueError as error:
            raise ValidationError(f"{path}.{key}[{index}] is not a valid CIDR: {value}") from error
        normalized = str(network)
        if normalized in seen:
            raise ValidationError(f"{path}.{key} contains duplicate CIDR {normalized}")
        seen.add(normalized)
    return cidrs


def validate_capability_catalog(
    catalog: dict[str, Any], requires_inference: bool, requires_tools: bool
) -> None:
    require_exact_keys(
        catalog,
        {"schemaVersion", "models", "tools", "catalogs"},
        "capabilityCatalog",
    )
    if catalog.get("schemaVersion") != "steward.capability-catalog/v2":
        raise ValidationError("capabilityCatalog.schemaVersion must equal steward.capability-catalog/v2")
    models = catalog.get("models")
    tools = catalog.get("tools")
    catalogs = catalog.get("catalogs")
    if not isinstance(models, list):
        raise ValidationError("capabilityCatalog.models must be an array")
    if not isinstance(tools, list):
        raise ValidationError("capabilityCatalog.tools must be an array")
    if not isinstance(catalogs, list):
        raise ValidationError("capabilityCatalog.catalogs must be an array")
    if not models and not tools:
        raise ValidationError("capabilityCatalog must contain at least one model or tool")

    model_keys: set[tuple[str, str]] = set()
    for index, model in enumerate(models):
        if not isinstance(model, dict):
            raise ValidationError(f"capabilityCatalog.models[{index}] must be an object")
        require_exact_keys(model, {"provider", "model"}, f"capabilityCatalog.models[{index}]")
        model_key = (
            require_string(model, "provider", f"capabilityCatalog.models[{index}]"),
            require_string(model, "model", f"capabilityCatalog.models[{index}]"),
        )
        if model_key in model_keys:
            raise ValidationError(
                f"capabilityCatalog.models contains duplicate entry {model_key[0]}/{model_key[1]}"
            )
        model_keys.add(model_key)

    tool_keys: set[tuple[str, str, str]] = set()
    for index, tool in enumerate(tools):
        if not isinstance(tool, dict):
            raise ValidationError(f"capabilityCatalog.tools[{index}] must be an object")
        require_exact_keys(
            tool,
            {"provider", "resource", "action", "accessClass"},
            f"capabilityCatalog.tools[{index}]",
        )
        tool_key = (
            require_string(tool, "provider", f"capabilityCatalog.tools[{index}]"),
            require_string(tool, "resource", f"capabilityCatalog.tools[{index}]"),
            require_string(tool, "action", f"capabilityCatalog.tools[{index}]"),
        )
        if tool_key in tool_keys:
            raise ValidationError(
                "capabilityCatalog.tools contains duplicate entry "
                f"{tool_key[0]}/{tool_key[1]}/{tool_key[2]}"
            )
        tool_keys.add(tool_key)
        access_class = require_string(tool, "accessClass", f"capabilityCatalog.tools[{index}]")
        if access_class not in {"read", "write", "destructive"}:
            raise ValidationError(
                f"capabilityCatalog.tools[{index}].accessClass must be read, write, or destructive"
            )

    catalog_keys: set[tuple[str, str]] = set()
    for index, provider_catalog in enumerate(catalogs):
        if not isinstance(provider_catalog, dict):
            raise ValidationError(f"capabilityCatalog.catalogs[{index}] must be an object")
        require_exact_keys(
            provider_catalog,
            {"provider", "catalogId", "version", "available"},
            f"capabilityCatalog.catalogs[{index}]",
        )
        catalog_key = (
            require_string(provider_catalog, "provider", f"capabilityCatalog.catalogs[{index}]"),
            require_string(provider_catalog, "catalogId", f"capabilityCatalog.catalogs[{index}]"),
        )
        require_string(provider_catalog, "version", f"capabilityCatalog.catalogs[{index}]")
        if not isinstance(provider_catalog.get("available"), bool):
            raise ValidationError(
                f"capabilityCatalog.catalogs[{index}].available must be a boolean"
            )
        if catalog_key in catalog_keys:
            raise ValidationError(
                "capabilityCatalog.catalogs contains duplicate entry "
                f"{catalog_key[0]}/{catalog_key[1]}"
            )
        catalog_keys.add(catalog_key)

    if requires_inference and not models:
        raise ValidationError("capabilityCatalog.models must be non-empty when the binding uses inference")
    if requires_tools and not tools:
        raise ValidationError("capabilityCatalog.tools must be non-empty when the binding uses tools")


def tagged_repository(reference: str, path: str) -> str:
    if any(character.isspace() for character in reference) or "@" in reference:
        raise ValidationError(f"{path} must be a tagged OCI reference")
    slash = reference.rfind("/")
    colon = reference.rfind(":")
    if colon <= slash or colon == len(reference) - 1:
        raise ValidationError(f"{path} must be a tagged OCI reference")
    return reference[:colon]


def hostname_covered(hostname: str, certificate_name: str) -> bool:
    host = hostname.lower().rstrip(".")
    name = certificate_name.lower().rstrip(".")
    if not name.startswith("*."):
        return host == name
    suffix = name[2:]
    if not host.endswith("." + suffix):
        return False
    prefix = host[: -(len(suffix) + 1)]
    return bool(prefix) and "." not in prefix


def validate_input(data: dict[str, Any]) -> list[dict[str, str]]:
    if data.get("schemaVersion") != CONTRACT:
        raise ValidationError(f"schemaVersion must equal {CONTRACT}")
    require_exact_keys(
        data,
        {
            "schemaVersion",
            "namespaces",
            "deploymentLock",
            "database",
            "gateway",
            "publicEndpoints",
            "providerProfiles",
            "arc",
            "externalSecrets",
            "externalConfigMaps",
            "execution",
            "browserAuth",
            "apiTlsSecretName",
            "webhookTlsSecretName",
            "capabilityCatalog",
            "networkPolicy",
        },
        "input",
    )

    namespaces = require_object(data, "namespaces", "input")
    require_exact_keys(namespaces, set(REQUIRED_NAMESPACES), "namespaces")
    for key in REQUIRED_NAMESPACES:
        validate_name(require_string(namespaces, key, "namespaces"), f"namespaces.{key}")

    lock = require_object(data, "deploymentLock", "input")
    require_exact_keys(
        lock,
        {
            "schemaVersion",
            "release",
            "mode",
            "requestedPlatform",
            "artifacts",
            "chartValues",
            "executionBindingImages",
        },
        "deploymentLock",
    )
    if lock.get("schemaVersion") != "steward.deployment-lock/v1":
        raise ValidationError("deploymentLock.schemaVersion must equal steward.deployment-lock/v1")
    release = require_object(lock, "release", "deploymentLock")
    require_exact_keys(release, {"version", "commit"}, "deploymentLock.release")
    require_string(release, "version", "deploymentLock.release")
    commit = require_string(release, "commit", "deploymentLock.release")
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValidationError("deploymentLock.release.commit must be a full lowercase Git SHA-1")
    mode = require_string(lock, "mode", "deploymentLock")
    if "requestedPlatform" not in lock:
        raise ValidationError("deploymentLock.requestedPlatform is required")
    requested_platform = lock.get("requestedPlatform")
    if mode == "index":
        if requested_platform is not None:
            raise ValidationError("deploymentLock.requestedPlatform must be null in index mode")
    elif mode == "single-platform":
        if not isinstance(requested_platform, str) or not re.fullmatch(
            r"[a-z0-9][a-z0-9._-]*/[a-z0-9][a-z0-9._-]*(?:/[a-z0-9][a-z0-9._-]*)?",
            requested_platform,
        ):
            raise ValidationError("deploymentLock.requestedPlatform must name the selected platform")
    else:
        raise ValidationError("deploymentLock.mode must be index or single-platform")
    artifacts = require_object(lock, "artifacts", "deploymentLock")
    chart_lock = require_object(lock, "chartValues", "deploymentLock")
    images = require_object(chart_lock, "images", "deploymentLock.chartValues")
    repository = require_string(images, "repository", "deploymentLock.chartValues.images")
    if "@" in repository or repository.endswith(":latest"):
        raise ValidationError("deploymentLock.chartValues.images.repository must not contain a tag or digest")
    for component in IMAGE_COMPONENTS:
        artifact = require_object(artifacts, f"images.{component}", "deploymentLock.artifacts")
        target = require_object(artifact, "target", f"deploymentLock.artifacts.images.{component}")
        target_reference = require_string(target, "reference", f"deploymentLock.artifacts.images.{component}.target")
        target_repository = tagged_repository(
            target_reference, f"deploymentLock.artifacts.images.{component}.target.reference"
        )
        target_digest = require_string(target, "digest", f"deploymentLock.artifacts.images.{component}.target")
        validate_digest(target_digest, f"deploymentLock.artifacts.images.{component}.target.digest")
        if component == "bridge":
            continue
        image = require_object(images, component, "deploymentLock.chartValues.images")
        tag = require_string(image, "tag", f"deploymentLock.chartValues.images.{component}")
        if tag == "latest":
            raise ValidationError(f"deploymentLock.chartValues.images.{component}.tag must not be latest")
        digest = require_string(image, "digest", f"deploymentLock.chartValues.images.{component}")
        validate_digest(digest, f"deploymentLock.chartValues.images.{component}.digest")
        if digest != target_digest:
            raise ValidationError(f"deploymentLock chartValues digest for {component} does not match artifacts target")
        if f"{repository}:{tag}" != target_reference:
            raise ValidationError(f"deploymentLock chartValues reference for {component} does not match artifacts target")
    bridge_values = require_object(chart_lock, "connectionsBridge", "deploymentLock.chartValues")
    bridge_image = require_string(bridge_values, "image", "deploymentLock.chartValues.connectionsBridge")
    bridge_target = require_object(artifacts["images.bridge"], "target", "deploymentLock.artifacts.images.bridge")
    bridge_reference = require_string(
        bridge_target, "reference", "deploymentLock.artifacts.images.bridge.target"
    )
    bridge_repository = tagged_repository(
        bridge_reference, "deploymentLock.artifacts.images.bridge.target.reference"
    )
    if bridge_image != f"{bridge_repository}@{bridge_target['digest']}":
        raise ValidationError("deploymentLock connectionsBridge image does not match artifacts target")
    binding_images = require_object(lock, "executionBindingImages", "deploymentLock")

    database = require_object(data, "database", "input")
    database_secret = require_object(database, "urlSecret", "database")
    for key in ("name", "key", "namespace"):
        validate_name(require_string(database_secret, key, "database.urlSecret"), f"database.urlSecret.{key}")
    if database_secret["namespace"] != namespaces["steward"]:
        raise ValidationError("database.urlSecret.namespace must equal namespaces.steward")

    tls = require_object(database, "tls", "database")
    mode = require_string(tls, "mode", "database.tls")
    if mode not in ("disabled", "verify-full"):
        raise ValidationError("database.tls.mode must be disabled or verify-full")
    ca = tls.get("ca")
    if mode == "verify-full":
        if not isinstance(ca, dict):
            raise ValidationError("database.tls.ca is required when mode is verify-full")
        if ca.get("kind") not in ("ConfigMap", "Secret"):
            raise ValidationError("database.tls.ca.kind must be ConfigMap or Secret")
        for key in ("name", "key", "namespace"):
            validate_name(require_string(ca, key, "database.tls.ca"), f"database.tls.ca.{key}")
        if ca["namespace"] != namespaces["steward"]:
            raise ValidationError("database.tls.ca.namespace must equal namespaces.steward because volume references are namespace-local")
        if ca["namespace"] != namespaces["databaseTls"]:
            raise ValidationError("database.tls.ca.namespace must equal namespaces.databaseTls")
    elif ca not in (None, {}):
        raise ValidationError("database.tls.ca must be omitted when mode is disabled")

    gateway = require_object(data, "gateway", "input")
    require_exact_keys(
        gateway,
        {"parentRef", "certificateNames", "tlsSecretName", "issuerRef", "backendTls"},
        "gateway",
    )
    parent = require_object(gateway, "parentRef", "gateway")
    for key in ("name", "namespace", "sectionName"):
        validate_name(require_string(parent, key, "gateway.parentRef"), f"gateway.parentRef.{key}")
    if parent["namespace"] != namespaces["gateway"]:
        raise ValidationError("gateway.parentRef.namespace must equal namespaces.gateway")

    validate_name(require_string(gateway, "tlsSecretName", "gateway"), "gateway.tlsSecretName")
    issuer = require_object(gateway, "issuerRef", "gateway")
    require_exact_keys(issuer, {"name", "kind"}, "gateway.issuerRef")
    for key in ("name", "kind"):
        require_string(issuer, key, "gateway.issuerRef")
    backend_tls = require_object(gateway, "backendTls", "gateway")
    require_exact_keys(backend_tls, {"caConfigMap"}, "gateway.backendTls")
    backend_ca = require_object(backend_tls, "caConfigMap", "gateway.backendTls")
    require_exact_keys(backend_ca, {"name", "namespace", "key"}, "gateway.backendTls.caConfigMap")
    validate_name(require_string(backend_ca, "name", "gateway.backendTls.caConfigMap"), "gateway.backendTls.caConfigMap.name")
    if require_string(backend_ca, "key", "gateway.backendTls.caConfigMap") != "ca.crt":
        raise ValidationError("gateway.backendTls.caConfigMap.key must equal ca.crt")
    if require_string(backend_ca, "namespace", "gateway.backendTls.caConfigMap") != namespaces["steward"]:
        raise ValidationError("gateway.backendTls.caConfigMap.namespace must equal namespaces.steward")

    certificate_names = gateway.get("certificateNames")
    if not isinstance(certificate_names, list) or not certificate_names:
        raise ValidationError("gateway.certificateNames must be a non-empty array")
    for index, name in enumerate(certificate_names):
        if not isinstance(name, str) or not DNS_NAME.fullmatch(name.removeprefix("*.")):
            raise ValidationError(f"gateway.certificateNames[{index}] is not an exact DNS name or wildcard")

    endpoints = data.get("publicEndpoints")
    if not isinstance(endpoints, list) or not endpoints:
        raise ValidationError("publicEndpoints must be a non-empty array")
    endpoint_names: set[str] = set()
    for index, endpoint in enumerate(endpoints):
        if not isinstance(endpoint, dict):
            raise ValidationError(f"publicEndpoints[{index}] must be an object")
        name = require_string(endpoint, "name", f"publicEndpoints[{index}]")
        hostname = require_string(endpoint, "hostname", f"publicEndpoints[{index}]").lower().rstrip(".")
        validate_name(name, f"publicEndpoints[{index}].name")
        if name in endpoint_names:
            raise ValidationError(f"publicEndpoints contains duplicate name {name}")
        endpoint_names.add(name)
        if not DNS_NAME.fullmatch(hostname):
            raise ValidationError(f"publicEndpoints[{index}].hostname is not a DNS name: {hostname}")
        if not any(hostname_covered(hostname, candidate) for candidate in certificate_names):
            joined = ", ".join(certificate_names)
            raise ValidationError(f"public endpoint {name} hostname {hostname} is not covered by certificate names: {joined}")
    if "steward" not in endpoint_names:
        raise ValidationError("publicEndpoints must contain the steward endpoint")

    profiles = data.get("providerProfiles")
    if not isinstance(profiles, list) or not profiles:
        raise ValidationError("providerProfiles must be a non-empty array")
    for index, profile in enumerate(profiles):
        if not isinstance(profile, dict):
            raise ValidationError(f"providerProfiles[{index}] must be an object")
        require_exact_keys(profile, {"name", "namespace", "inputs"}, f"providerProfiles[{index}]")
        validate_name(require_string(profile, "name", f"providerProfiles[{index}]"), f"providerProfiles[{index}].name")
        namespace = require_string(profile, "namespace", f"providerProfiles[{index}]")
        if namespace != namespaces["providers"]:
            raise ValidationError(f"providerProfiles[{index}].namespace must equal namespaces.providers")
        inputs = require_object(profile, "inputs", f"providerProfiles[{index}]")
        require_exact_keys(
            inputs,
            {"gatewayOrigin", "runtimeGrantOrigin", "serviceCidrs"},
            f"providerProfiles[{index}].inputs",
        )
        for key in ("gatewayOrigin", "runtimeGrantOrigin"):
            value = require_string(inputs, key, f"providerProfiles[{index}].inputs")
            if not value.startswith("https://"):
                raise ValidationError(f"providerProfiles[{index}].inputs.{key} must be an HTTPS origin")
        cidrs = inputs.get("serviceCidrs")
        if not isinstance(cidrs, list) or not cidrs or not all(isinstance(value, str) and value for value in cidrs):
            raise ValidationError(f"providerProfiles[{index}].inputs.serviceCidrs must be a non-empty string array")

    arc = require_object(data, "arc", "input")
    identity = require_object(arc, "controllerServiceAccount", "arc")
    for key in ("name", "namespace"):
        validate_name(require_string(identity, key, "arc.controllerServiceAccount"), f"arc.controllerServiceAccount.{key}")
    if identity["namespace"] != namespaces["arc"]:
        raise ValidationError("arc.controllerServiceAccount.namespace must equal namespaces.arc")

    external = require_object(data, "externalSecrets", "input")
    for purpose in ("database", "mint", "litellm", "openshellClient"):
        reference = require_object(external, purpose, "externalSecrets")
        validate_name(require_string(reference, "name", f"externalSecrets.{purpose}"), f"externalSecrets.{purpose}.name")
        namespace = require_string(reference, "namespace", f"externalSecrets.{purpose}")
        if namespace != namespaces["steward"]:
            raise ValidationError(f"externalSecrets.{purpose}.namespace must equal namespaces.steward")
        forbidden = set(reference) - {"name", "namespace"}
        if forbidden:
            raise ValidationError(f"externalSecrets.{purpose} may contain names only, not secret material: {sorted(forbidden)}")
    if external["database"]["name"] != database_secret["name"]:
        raise ValidationError("externalSecrets.database.name must equal database.urlSecret.name")

    network_policy = require_object(data, "networkPolicy", "input")
    require_exact_keys(network_policy, {"kubeApiCidrs", "postgresCidrs"}, "networkPolicy")
    validate_required_cidrs(network_policy, "kubeApiCidrs", "networkPolicy")
    validate_required_cidrs(network_policy, "postgresCidrs", "networkPolicy")

    execution = require_object(data, "execution", "input")
    endpoints = require_object(execution, "endpoints", "execution")
    for key in (
        "inference",
        "mcpGateway",
        "openshell",
        "workloadExchange",
        "litellm",
        "mintIssuer",
    ):
        value = require_string(endpoints, key, "execution.endpoints")
        if not value.startswith("https://"):
            raise ValidationError(f"execution.endpoints.{key} must use HTTPS")
    for key in ("openshellServerName", "workloadExchangeServerName", "spiffeTrustDomain"):
        require_string(endpoints, key, "execution.endpoints")
    bridge = require_object(execution, "connectionsBridge", "execution")
    bridge_origin = require_string(bridge, "mcpGatewayOrigin", "execution.connectionsBridge")
    if not bridge_origin.startswith("https://"):
        raise ValidationError("execution.connectionsBridge.mcpGatewayOrigin must use HTTPS")
    require_exact_keys(bridge, {"mcpGatewayOrigin", "mcpGatewayAuthorityContract"}, "execution.connectionsBridge")
    if require_string(bridge, "mcpGatewayAuthorityContract", "execution.connectionsBridge") not in (
        "steward.connections.github/v1",
        "steward.connections.github/v2",
    ):
        raise ValidationError("execution.connectionsBridge.mcpGatewayAuthorityContract is unsupported")
    binding = require_object(execution, "binding", "execution")
    for key in ("agentRef", "displayName", "adapter", "executable", "expectedVersion", "runtimeName"):
        require_string(binding, key, "execution.binding")
    runtime_image = binding_images.get(binding["runtimeName"])
    if not isinstance(runtime_image, str) or not re.fullmatch(r"[^\s@]+@sha256:[0-9a-f]{64}", runtime_image):
        raise ValidationError("execution.binding.runtimeName must select one immutable deploymentLock.executionBindingImages entry")
    runtime_artifact = require_object(
        artifacts,
        f"referenceRuntimes.{binding['runtimeName']}",
        "deploymentLock.artifacts",
    )
    runtime_target = require_object(
        runtime_artifact,
        "target",
        f"deploymentLock.artifacts.referenceRuntimes.{binding['runtimeName']}",
    )
    runtime_reference = require_string(
        runtime_target,
        "reference",
        f"deploymentLock.artifacts.referenceRuntimes.{binding['runtimeName']}.target",
    )
    runtime_repository = tagged_repository(
        runtime_reference,
        f"deploymentLock.artifacts.referenceRuntimes.{binding['runtimeName']}.target.reference",
    )
    runtime_digest = require_string(
        runtime_target,
        "digest",
        f"deploymentLock.artifacts.referenceRuntimes.{binding['runtimeName']}.target",
    )
    validate_digest(
        runtime_digest,
        f"deploymentLock.artifacts.referenceRuntimes.{binding['runtimeName']}.target.digest",
    )
    if runtime_image != f"{runtime_repository}@{runtime_digest}":
        raise ValidationError(
            f"deploymentLock runtime image for {binding['runtimeName']} does not match artifacts target"
        )
    profile_ids = {profile["name"] for profile in profiles}
    for key in ("toolsProfile", "inferenceProfile"):
        if key not in binding:
            continue
        selected = require_string(binding, key, "execution.binding")
        if selected not in profile_ids:
            raise ValidationError(f"execution.binding.{key} must select one providerProfiles name")

    validate_capability_catalog(
        require_object(data, "capabilityCatalog", "input"),
        requires_inference="inferenceProfile" in binding,
        requires_tools="toolsProfile" in binding,
    )

    config_maps = require_object(data, "externalConfigMaps", "input")
    workload_trust = require_object(config_maps, "workloadExchangeTrust", "externalConfigMaps")
    for key in ("name", "namespace", "key"):
        validate_name(require_string(workload_trust, key, "externalConfigMaps.workloadExchangeTrust"), f"externalConfigMaps.workloadExchangeTrust.{key}")
    if workload_trust["namespace"] != namespaces["steward"]:
        raise ValidationError("externalConfigMaps.workloadExchangeTrust.namespace must equal namespaces.steward")

    return [{"code": "input.valid", "severity": "info", "message": "deployment input relationships are valid"}]


def steward_endpoint(data: dict[str, Any]) -> dict[str, str]:
    for endpoint in data["publicEndpoints"]:
        if endpoint["name"] == "steward":
            return endpoint
    raise AssertionError("validated input has no steward endpoint")


def chart_values(data: dict[str, Any], profile_digests: dict[str, str]) -> dict[str, Any]:
    lock = data["deploymentLock"]
    endpoint = steward_endpoint(data)
    parent = data["gateway"]["parentRef"]
    backend_tls = data["gateway"]["backendTls"]
    database = data["database"]
    external = data["externalSecrets"]
    chart_lock = lock["chartValues"]
    images = chart_lock["images"]
    execution = data["execution"]
    endpoints = execution["endpoints"]
    binding = execution["binding"]
    values: dict[str, Any] = {
        "images": images,
        "execution": {"enabled": True},
        "web": {
            "enabled": True,
            "host": endpoint["hostname"],
            "httpRoute": {
                "enabled": True,
                "parentRefs": [parent],
                "hostname": endpoint["hostname"],
                "apiPaths": [{"type": "PathPrefix", "value": "/api"}],
                "webPaths": [{"type": "PathPrefix", "value": "/"}],
                "backendTls": {
                    "hostname": f"steward-apiserver.{data['namespaces']['steward']}.svc.cluster.local",
                    "caConfigMap": {
                        "name": backend_tls["caConfigMap"]["name"],
                        "key": backend_tls["caConfigMap"]["key"],
                    },
                },
            },
        },
        "browserAuth": {
            "enabled": True,
            "google": {
                "clientId": data["browserAuth"]["clientId"],
                "origin": f"https://{endpoint['hostname']}",
                "workspaceDomain": data["browserAuth"]["workspaceDomain"],
                "organizationId": data["browserAuth"]["organizationId"],
                "clientSecret": {"name": data["browserAuth"]["clientSecretName"], "key": data["browserAuth"]["clientSecretKey"]},
            },
        },
        "secrets": {
            "database": {"name": external["database"]["name"], "key": database["urlSecret"]["key"]},
            "mint": {"name": external["mint"]["name"], "signingKey": "signing-key", "introspectionCredential": "introspection-credential"},
            "litellm": {"name": external["litellm"]["name"], "key": "master-key"},
            "openshellClient": {"name": external["openshellClient"]["name"], "caCertificate": "ca.crt", "clientCertificate": "tls.crt", "clientPrivateKey": "tls.key"},
        },
        "databaseTls": {"mode": database["tls"]["mode"], "ca": {"kind": "ConfigMap", "name": "", "key": ""}},
        "tls": {
            "mode": "certManager",
            "api": {"secretName": data["apiTlsSecretName"]},
            "issuerRef": data["gateway"]["issuerRef"],
            "webhook": {"secretName": data["webhookTlsSecretName"], "caBundlePem": ""},
        },
        "runtimeNamespaces": [data["namespaces"]["runtime"]],
        "connectionsBridge": {
            "enabled": True,
            "artifactTrust": {"mode": "operator-pinned"},
            "image": chart_lock["connectionsBridge"]["image"],
            "mcpGatewayOrigin": execution["connectionsBridge"]["mcpGatewayOrigin"],
            "mcpGatewayAuthorityContract": execution["connectionsBridge"]["mcpGatewayAuthorityContract"],
            "mcpGatewayVersion": "",
            "runtimeNamespace": data["namespaces"]["runtime"],
        },
        "workloadExchangeTrust": {
            "kind": "ConfigMap",
            "name": data["externalConfigMaps"]["workloadExchangeTrust"]["name"],
            "caCertificate": data["externalConfigMaps"]["workloadExchangeTrust"]["key"],
        },
        "config": {
            "taskOrchestrationMode": "active",
            "apiserver": {
                "capabilityCatalog": data["capabilityCatalog"],
                "inferenceEndpoint": endpoints["inference"],
                "mcpGatewayEndpoint": endpoints["mcpGateway"],
                "executionBindingsMode": "active",
                "executionBindings": {
                    "apiVersion": "steward.execution-bindings/v1",
                    "bindings": [
                        {
                            "agentRef": binding["agentRef"],
                            "displayName": binding["displayName"],
                            "adapter": binding["adapter"],
                            "image": lock["executionBindingImages"][binding["runtimeName"]],
                            "executable": binding["executable"],
                            "versionProbe": {"arguments": ["--version"], "expectedStdout": binding["expectedVersion"]},
                            "providerProfiles": {
                                key.removesuffix("Profile"): {
                                    "id": binding[key],
                                    "digest": profile_digests[binding[key]],
                                }
                                for key in ("toolsProfile", "inferenceProfile")
                                if key in binding
                            },
                        }
                    ],
                },
            },
            "controller": {
                "openshellEndpoint": endpoints["openshell"],
                "openshellServerName": endpoints["openshellServerName"],
                "workloadExchangeEndpoint": endpoints["workloadExchange"],
                "workloadExchangeServerName": endpoints["workloadExchangeServerName"],
                "litellmUrl": endpoints["litellm"],
            },
            "mint": {
                "issuer": endpoints["mintIssuer"],
                "spiffeTrustDomain": endpoints["spiffeTrustDomain"],
                "openshellNamespace": data["namespaces"]["openshell"],
            },
        },
        "networkPolicy": {
            "enabled": True,
            "dnsNamespace": data["namespaces"]["dns"],
            "ingressNamespace": data["namespaces"]["gateway"],
            "arcNamespace": data["namespaces"]["arc"],
            "mcpGatewayNamespace": data["namespaces"]["mcpGateway"],
            "litellmNamespace": data["namespaces"]["litellm"],
            "identityExchangeNamespace": data["namespaces"]["identityExchange"],
            "openshellNamespace": data["namespaces"]["openshell"],
            "apiserverIngressNamespaces": [data["namespaces"]["arc"]],
            "browserAuthEgressCidrs": data["browserAuth"]["egressCidrs"],
            "kubeApiCidrs": data["networkPolicy"]["kubeApiCidrs"],
            "postgresCidrs": data["networkPolicy"]["postgresCidrs"],
        },
    }
    if database["tls"]["mode"] == "verify-full":
        ca = database["tls"]["ca"]
        values["databaseTls"]["ca"] = {"kind": ca["kind"], "name": ca["name"], "key": ca["key"]}
    return values


def provider_profile_inputs(data: dict[str, Any]) -> dict[str, Any]:
    return {
        "schema": "steward.provider-profile-inputs/v1",
        "bundle": {"id": "steward-runtime-providers", "version": "1.2.0"},
        "profiles": [
            {
                "id": profile["name"],
                "inputs": {
                    "gateway-origin": profile["inputs"]["gatewayOrigin"],
                    "runtime-grant-origin": profile["inputs"]["runtimeGrantOrigin"],
                    "service-cidrs": profile["inputs"]["serviceCidrs"],
                },
            }
            for profile in data["providerProfiles"]
        ],
    }


def resolve_provider_profile_digests(
    data: dict[str, Any], bundle: pathlib.Path, inputs: dict[str, Any]
) -> dict[str, str]:
    installer = bundle / "bin" / "steward-provider-profile"
    if not installer.is_file():
        raise ValidationError(
            f"released provider-profile installer is required at {installer}"
        )
    with tempfile.TemporaryDirectory(prefix="steward-provider-profile-") as temporary:
        inputs_path = pathlib.Path(temporary) / "inputs.json"
        inputs_path.write_text(canonical(inputs), encoding="utf-8")
        result = subprocess.run(
            [
                str(installer),
                "validate",
                "--bundle",
                str(bundle),
                "--inputs",
                str(inputs_path),
            ],
            check=False,
            capture_output=True,
            text=True,
        )
    if result.returncode != 0:
        raise ValidationError(
            f"released provider-profile validation failed: {(result.stderr or result.stdout).strip()}"
        )
    try:
        report = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise ValidationError(
            f"released provider-profile installer returned invalid JSON: {error}"
        ) from error
    if not isinstance(report, dict):
        raise ValidationError("released provider-profile installer returned a non-object result")
    if (
        report.get("schemaVersion") != "steward.provider-profile-result/v1"
        or report.get("operation") != "validate"
        or report.get("status") != "valid"
        or report.get("bundle")
        != {"id": "steward-runtime-providers", "version": "1.2.0"}
    ):
        raise ValidationError("released provider-profile installer returned an unexpected contract")
    reported_profiles = report.get("profiles")
    if not isinstance(reported_profiles, list):
        raise ValidationError("released provider-profile result requires profiles")
    digests: dict[str, str] = {}
    for index, profile in enumerate(reported_profiles):
        if not isinstance(profile, dict):
            raise ValidationError(f"released provider-profile result profiles[{index}] must be an object")
        profile_id = require_string(profile, "id", f"provider-profile result profiles[{index}]")
        digest = require_string(profile, "digest", f"provider-profile result profiles[{index}]")
        validate_digest(digest, f"provider-profile result profiles[{index}].digest")
        if profile_id in digests:
            raise ValidationError(f"released provider-profile result repeats {profile_id}")
        digests[profile_id] = digest
    expected = {profile["name"] for profile in data["providerProfiles"]}
    if set(digests) != expected:
        raise ValidationError(
            f"released provider-profile result must exactly match requested profiles; "
            f"expected={sorted(expected)}, actual={sorted(digests)}"
        )
    return digests


def namespace_references(data: dict[str, Any]) -> dict[str, Any]:
    tls = data["database"]["tls"]
    references: dict[str, Any] = {
        "schemaVersion": "steward.namespace-references/v1",
        "namespaceMap": data["namespaces"],
        "gatewayParentRef": data["gateway"]["parentRef"],
        "arcControllerServiceAccount": data["arc"]["controllerServiceAccount"],
        "externalSecrets": data["externalSecrets"],
        "externalConfigMaps": data["externalConfigMaps"],
        "providerProfiles": [
            {"name": profile["name"], "namespace": profile["namespace"]}
            for profile in data["providerProfiles"]
        ],
    }
    if tls["mode"] == "verify-full":
        references["databaseCa"] = tls["ca"]
    return references


def validate_browser(data: dict[str, Any]) -> None:
    browser = require_object(data, "browserAuth", "input")
    for key in ("clientId", "workspaceDomain", "organizationId", "clientSecretName", "clientSecretKey"):
        require_string(browser, key, "browserAuth")
    egress_cidrs = browser.get("egressCidrs")
    if not isinstance(egress_cidrs, list) or not egress_cidrs or not all(isinstance(value, str) and value for value in egress_cidrs):
        raise ValidationError("browserAuth.egressCidrs must be a non-empty string array")
    for key in ("tlsSecretName",):
        validate_name(require_string(data["gateway"], key, "gateway"), f"gateway.{key}")
    issuer = require_object(data["gateway"], "issuerRef", "gateway")
    validate_name(require_string(issuer, "name", "gateway.issuerRef"), "gateway.issuerRef.name")
    if issuer.get("kind") not in ("Issuer", "ClusterIssuer"):
        raise ValidationError("gateway.issuerRef.kind must be Issuer or ClusterIssuer")
    validate_name(require_string(data, "apiTlsSecretName", "input"), "apiTlsSecretName")
    validate_name(require_string(data, "webhookTlsSecretName", "input"), "webhookTlsSecretName")


def run_helm(chart: pathlib.Path, namespace: str, values_path: pathlib.Path) -> None:
    helm = shutil.which("helm")
    if helm is None:
        raise ValidationError("helm is required to validate generated values")
    for command in (
        [helm, "lint", str(chart), "--values", str(values_path)],
        [helm, "template", "steward", str(chart), "--namespace", namespace, "--values", str(values_path)],
    ):
        result = subprocess.run(command, check=False, capture_output=True, text=True)
        if result.returncode != 0:
            detail = (result.stderr or result.stdout).strip()
            raise ValidationError(f"chart validation failed: {detail}")


def flux_configmap(namespace: str, values: dict[str, Any], digest: str) -> str:
    indented = "".join("    " + line for line in canonical(values).splitlines(keepends=True))
    return (
        "apiVersion: v1\nkind: ConfigMap\nmetadata:\n"
        f"  name: steward-platform-values-{digest[7:19]}\n  namespace: {namespace}\n"
        "data:\n  values.json: |\n" + indented
    )


def build_result(
    data: dict[str, Any],
    diagnostics: list[dict[str, str]],
    values: dict[str, Any],
    profile_digests: dict[str, str],
) -> dict[str, Any]:
    digest = "sha256:" + hashlib.sha256(canonical(values).encode()).hexdigest()
    return {
        "schemaVersion": RESULT_CONTRACT,
        "status": "valid",
        "inputContract": CONTRACT,
        "valuesDigest": digest,
        "namespaces": data["namespaces"],
        "gatewayParentRef": data["gateway"]["parentRef"],
        "providerProfiles": [
            {
                "name": profile["name"],
                "namespace": profile["namespace"],
                "digest": profile_digests[profile["name"]],
            }
            for profile in data["providerProfiles"]
        ],
        "arcControllerServiceAccount": data["arc"]["controllerServiceAccount"],
        "diagnostics": diagnostics,
    }


def generate(args: argparse.Namespace) -> int:
    data = read_json(args.input)
    diagnostics = validate_input(data)
    validate_browser(data)
    profile_inputs = provider_profile_inputs(data)
    profile_digests = resolve_provider_profile_digests(
        data, args.provider_profile_bundle, profile_inputs
    )
    diagnostics.append(
        {
            "code": "provider-profiles.valid",
            "severity": "info",
            "message": "released provider-profile renderer resolved the exact binding digests",
        }
    )
    values = chart_values(data, profile_digests)
    args.output.mkdir(parents=True, exist_ok=False)
    values_path = args.output / "steward-values.json"
    values_path.write_text(canonical(values), encoding="utf-8")
    (args.output / "provider-profile-inputs.json").write_text(canonical(profile_inputs), encoding="utf-8")
    (args.output / "namespace-references.json").write_text(canonical(namespace_references(data)), encoding="utf-8")
    if args.chart is not None:
        run_helm(args.chart, data["namespaces"]["steward"], values_path)
        diagnostics.append({"code": "chart.valid", "severity": "info", "message": "generated values pass Helm lint and template"})
    result = build_result(data, diagnostics, values, profile_digests)
    (args.output / "diagnostics.json").write_text(canonical(result), encoding="utf-8")
    (args.output / "flux-values-configmap.yaml").write_text(flux_configmap(data["namespaces"]["steward"], values, result["valuesDigest"]), encoding="utf-8")
    summary = (
        "Steward platform preflight: valid\n"
        f"values digest: {result['valuesDigest']}\n"
        f"Steward namespace: {data['namespaces']['steward']}\n"
        f"runtime namespace: {data['namespaces']['runtime']}\n"
        f"public endpoint: {steward_endpoint(data)['hostname']}\n"
        f"Gateway: {data['gateway']['parentRef']['namespace']}/{data['gateway']['parentRef']['name']}\n"
        f"Capability models: {len(data['capabilityCatalog']['models'])}\n"
        f"Capability tools: {len(data['capabilityCatalog']['tools'])}\n"
        f"Execution bindings: {len(values['config']['apiserver']['executionBindings']['bindings'])}\n"
        f"Runtime namespaces: {len(values['runtimeNamespaces'])}\n"
        f"API caller namespaces: {len(values['networkPolicy']['apiserverIngressNamespaces'])}\n"
        f"Kubernetes API CIDRs: {len(data['networkPolicy']['kubeApiCidrs'])}\n"
        f"PostgreSQL CIDRs: {len(data['networkPolicy']['postgresCidrs'])}\n"
        "Secret bodies emitted: no\n"
    )
    (args.output / "summary.txt").write_text(summary, encoding="utf-8")
    print(canonical(result), end="")
    return 0


def validate_command(args: argparse.Namespace) -> int:
    data = read_json(args.input)
    diagnostics = validate_input(data)
    validate_browser(data)
    profile_inputs = provider_profile_inputs(data)
    profile_digests = resolve_provider_profile_digests(
        data, args.provider_profile_bundle, profile_inputs
    )
    diagnostics.append(
        {
            "code": "provider-profiles.valid",
            "severity": "info",
            "message": "released provider-profile renderer resolved the exact binding digests",
        }
    )
    values = chart_values(data, profile_digests)
    with tempfile.TemporaryDirectory(prefix="steward-platform-preflight-") as temporary:
        values_path = pathlib.Path(temporary) / "values.json"
        values_path.write_text(canonical(values), encoding="utf-8")
        if args.chart is not None:
            run_helm(args.chart, data["namespaces"]["steward"], values_path)
            diagnostics.append({"code": "chart.valid", "severity": "info", "message": "generated values pass Helm lint and template"})
    print(canonical(build_result(data, diagnostics, values, profile_digests)), end="")
    return 0


def kubectl_metadata_check(
    args: argparse.Namespace, resource: str, namespace: str, name: str, label: str
) -> dict[str, str]:
    command = [
        args.kubectl,
        "--kubeconfig",
        str(args.kubeconfig),
        "--context",
        args.context,
        "get",
        resource,
        name,
        "--namespace",
        namespace,
        "--output",
        "jsonpath={.metadata.namespace}/{.metadata.name}",
    ]
    result = subprocess.run(command, check=False, capture_output=True, text=True)
    if result.returncode != 0:
        detail = (result.stderr or result.stdout).strip()
        raise ValidationError(f"{label} {namespace}/{name} does not exist or is unreadable: {detail}")
    actual = result.stdout.strip()
    if actual != f"{namespace}/{name}":
        raise ValidationError(f"{label} identity {actual or '<empty>'} does not equal configured reference {namespace}/{name}")
    return {
        "code": "reference.live",
        "severity": "info",
        "message": f"{label} {namespace}/{name} exists",
    }


def gateway_check(args: argparse.Namespace) -> int:
    data = read_json(args.input)
    diagnostics = validate_input(data)
    validate_browser(data)
    parent = data["gateway"]["parentRef"]
    command = [
        args.kubectl,
        "--kubeconfig",
        str(args.kubeconfig),
        "--context",
        args.context,
        "get",
        "gateway.gateway.networking.k8s.io",
        parent["name"],
        "--namespace",
        parent["namespace"],
        "--output",
        "json",
    ]
    result = subprocess.run(command, check=False, capture_output=True, text=True)
    if result.returncode != 0:
        detail = (result.stderr or result.stdout).strip()
        raise ValidationError(
            f"Gateway {parent['namespace']}/{parent['name']} does not exist or is unreadable: {detail}"
        )
    try:
        gateway = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise ValidationError(f"kubectl returned invalid Gateway JSON: {error}") from error
    metadata = gateway.get("metadata", {})
    if gateway.get("apiVersion") != "gateway.networking.k8s.io/v1" or gateway.get("kind") != "Gateway":
        raise ValidationError("live object is not a gateway.networking.k8s.io/v1 Gateway")
    if metadata.get("namespace") != parent["namespace"] or metadata.get("name") != parent["name"]:
        raise ValidationError(
            f"live Gateway identity {metadata.get('namespace')}/{metadata.get('name')} does not equal configured parent {parent['namespace']}/{parent['name']}"
        )
    listeners = gateway.get("spec", {}).get("listeners", [])
    listener = next((item for item in listeners if item.get("name") == parent["sectionName"]), None)
    if listener is None:
        raise ValidationError(
            f"Gateway {parent['namespace']}/{parent['name']} has no listener named {parent['sectionName']}"
        )
    if listener.get("protocol") != "HTTPS":
        raise ValidationError(f"Gateway listener {parent['sectionName']} must use HTTPS")
    listener_hostname = listener.get("hostname")
    if listener_hostname:
        for endpoint in data["publicEndpoints"]:
            if not hostname_covered(endpoint["hostname"], listener_hostname):
                raise ValidationError(
                    f"Gateway listener hostname {listener_hostname} does not admit {endpoint['name']} hostname {endpoint['hostname']}"
                )
    certificate_refs = listener.get("tls", {}).get("certificateRefs", [])
    expected_secret = data["gateway"]["tlsSecretName"]
    if not any(
        reference.get("name") == expected_secret
        and reference.get("kind", "Secret") == "Secret"
        and reference.get("group", "") in ("", "core")
        and reference.get("namespace", parent["namespace"]) == parent["namespace"]
        for reference in certificate_refs
    ):
        raise ValidationError(
            f"Gateway listener {parent['sectionName']} does not reference TLS Secret {expected_secret}"
        )
    diagnostics.extend(
        [
            {
                "code": "gateway.live",
                "severity": "info",
                "message": f"live Gateway {parent['namespace']}/{parent['name']} and listener {parent['sectionName']} match",
            },
            {
                "code": "certificate.coverage",
                "severity": "info",
                "message": "every enabled public hostname is covered by the declared certificate names",
            },
        ]
    )
    arc = data["arc"]["controllerServiceAccount"]
    diagnostics.append(
        kubectl_metadata_check(
            args,
            "serviceaccount",
            arc["namespace"],
            arc["name"],
            "ARC controller ServiceAccount",
        )
    )
    diagnostics.append(
        kubectl_metadata_check(
            args,
            "secret",
            parent["namespace"],
            data["gateway"]["tlsSecretName"],
            "Gateway TLS Secret",
        )
    )
    for purpose, reference in sorted(data["externalSecrets"].items()):
        diagnostics.append(
            kubectl_metadata_check(
                args,
                "secret",
                reference["namespace"],
                reference["name"],
                f"{purpose} Secret",
            )
        )
    for purpose, reference in sorted(data["externalConfigMaps"].items()):
        diagnostics.append(
            kubectl_metadata_check(
                args,
                "configmap",
                reference["namespace"],
                reference["name"],
                f"{purpose} ConfigMap",
            )
        )
    tls = data["database"]["tls"]
    if tls["mode"] == "verify-full":
        ca = tls["ca"]
        diagnostics.append(
            kubectl_metadata_check(
                args,
                ca["kind"].lower(),
                ca["namespace"],
                ca["name"],
                "database CA source",
            )
        )
    print(
        canonical(
            {
                "schemaVersion": RESULT_CONTRACT,
                "status": "valid",
                "operation": "gateway-check",
                "diagnostics": diagnostics,
            }
        ),
        end="",
    )
    return 0


def kubectl_base(args: argparse.Namespace) -> list[str]:
    return [args.kubectl, "--kubeconfig", str(args.kubeconfig), "--context", args.context]


def network_policy_agent(args: argparse.Namespace) -> tuple[dict[str, Any], str]:
    command = kubectl_base(args) + [
        "get",
        "daemonset",
        "aws-node",
        "--namespace",
        "kube-system",
        "--output",
        "json",
    ]
    result = subprocess.run(command, check=False, capture_output=True, text=True)
    if result.returncode != 0:
        detail = (result.stderr or result.stdout).strip()
        raise ValidationError(f"cannot read kube-system/aws-node DaemonSet: {detail}")
    try:
        daemonset = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise ValidationError(f"kubectl returned invalid aws-node DaemonSet JSON: {error}") from error
    containers = daemonset.get("spec", {}).get("template", {}).get("spec", {}).get("containers", [])
    agent = next((container for container in containers if container.get("name") == "aws-network-policy-agent"), None)
    if agent is None:
        raise ValidationError("kube-system/aws-node has no aws-network-policy-agent container")
    arguments = agent.get("args", [])
    environment = {item.get("name"): item.get("value") for item in agent.get("env", [])}
    enabled = "--enable-network-policy=true" in arguments or environment.get("ENABLE_NETWORK_POLICY", "").lower() == "true"
    if not enabled:
        raise ValidationError("aws-network-policy-agent is present but --enable-network-policy=true is not observable")
    image = agent.get("image")
    if not isinstance(image, str) or not image:
        raise ValidationError("aws-network-policy-agent image is not observable")
    return daemonset, image


def network_check(args: argparse.Namespace) -> int:
    _, image = network_policy_agent(args)
    result = {
        "schemaVersion": RESULT_CONTRACT,
        "status": "preflight-valid",
        "operation": "network-check",
        "cni": {
            "implementation": "Amazon VPC CNI",
            "daemonSet": "kube-system/aws-node",
            "networkPolicyAgentImage": image,
            "networkPolicyEnabled": True,
        },
        "diagnostics": [
            {
                "code": "cni.network-policy-agent-observed",
                "severity": "info",
                "message": "aws-network-policy-agent is enabled; an actual deny/allow smoke is still required",
            }
        ],
    }
    print(canonical(result), end="")
    return 0


def run_kubectl(args: argparse.Namespace, arguments: list[str], stdin: str | None = None) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        kubectl_base(args) + arguments,
        input=stdin,
        check=False,
        capture_output=True,
        text=True,
    )


def apply_object(args: argparse.Namespace, value: dict[str, Any]) -> None:
    result = run_kubectl(args, ["apply", "--filename", "-"], canonical(value))
    if result.returncode != 0:
        raise ValidationError(f"kubectl apply failed: {(result.stderr or result.stdout).strip()}")


def create_object(args: argparse.Namespace, value: dict[str, Any]) -> None:
    result = run_kubectl(args, ["create", "--filename", "-"], canonical(value))
    if result.returncode != 0:
        raise ValidationError(f"kubectl create failed: {(result.stderr or result.stdout).strip()}")


def namespace_identity(args: argparse.Namespace, namespace: str) -> tuple[str, str]:
    result = run_kubectl(args, ["get", "namespace", namespace, "--output", "json"])
    if result.returncode != 0:
        raise ValidationError(
            f"cannot read run-owned namespace {namespace}: {(result.stderr or result.stdout).strip()}"
        )
    try:
        value = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise ValidationError(f"kubectl returned invalid Namespace JSON: {error}") from error
    metadata = value.get("metadata")
    if not isinstance(metadata, dict):
        raise ValidationError(f"namespace {namespace} has no metadata")
    uid = metadata.get("uid")
    labels = metadata.get("labels")
    run_id = labels.get("steward.test/run-id") if isinstance(labels, dict) else None
    if not isinstance(uid, str) or not uid:
        raise ValidationError(f"namespace {namespace} has no immutable UID")
    if not isinstance(run_id, str) or not run_id:
        raise ValidationError(f"namespace {namespace} has no run ownership label")
    return uid, run_id


def network_smoke(args: argparse.Namespace) -> int:
    network_policy_agent(args)
    if not re.fullmatch(r"[a-z0-9](?:[a-z0-9-]{0,28}[a-z0-9])?", args.run_id):
        raise ValidationError("run-id must be 1-30 lowercase alphanumeric or hyphen characters")
    if not re.fullmatch(r"[^\s@]+@sha256:[0-9a-f]{64}", args.probe_image):
        raise ValidationError("probe-image must be an immutable image@sha256 digest")
    if args.probe_image.endswith("sha256:" + "0" * 64):
        raise ValidationError("probe-image must not use an all-zero placeholder digest")
    namespace = f"steward-netpol-{args.run_id}"
    labels = {"steward.test/run-id": args.run_id, "app.kubernetes.io/part-of": "steward-network-smoke"}
    namespace_created = False
    namespace_uid: str | None = None
    denied = False
    allowed = False
    cleanup_error: str | None = None
    cleanup_signals = {signal.SIGINT, signal.SIGTERM}
    previous_signal_mask = signal.pthread_sigmask(signal.SIG_BLOCK, cleanup_signals)
    previous_signal_handlers = install_cleanup_signal_handlers()
    cleanup_signals_unblocked = False
    try:
        create_object(
            args,
            {"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": namespace, "labels": labels}},
        )
        namespace_created = True
        namespace_uid, observed_run_id = namespace_identity(args, namespace)
        if observed_run_id != args.run_id:
            raise ValidationError(
                f"created namespace {namespace} ownership label {observed_run_id} does not equal {args.run_id}"
            )
        signal.pthread_sigmask(signal.SIG_SETMASK, previous_signal_mask)
        cleanup_signals_unblocked = True
        for name, role, command in (
            ("server", "server", ["sh", "-c", "mkdir -p /tmp/www && printf ready >/tmp/www/index.html && httpd -f -p 8080 -h /tmp/www"]),
            ("client", "client", ["sh", "-c", "trap : TERM INT; sleep 3600 & wait"]),
        ):
            apply_object(
                args,
                {
                    "apiVersion": "v1",
                    "kind": "Pod",
                    "metadata": {"name": name, "namespace": namespace, "labels": {**labels, "role": role}},
                    "spec": {
                        "restartPolicy": "Never",
                        "containers": [
                            {
                                "name": role,
                                "image": args.probe_image,
                                "imagePullPolicy": "IfNotPresent",
                                "command": command,
                                "securityContext": {
                                    "allowPrivilegeEscalation": False,
                                    "capabilities": {"drop": ["ALL"]},
                                    "runAsNonRoot": True,
                                    "runAsUser": 65532,
                                    "seccompProfile": {"type": "RuntimeDefault"},
                                },
                            }
                        ],
                    },
                },
            )
        apply_object(
            args,
            {
                "apiVersion": "v1",
                "kind": "Service",
                "metadata": {"name": "server", "namespace": namespace, "labels": labels},
                "spec": {"selector": {"role": "server"}, "ports": [{"name": "http", "protocol": "TCP", "port": 8080, "targetPort": 8080}]},
            },
        )
        for pod in ("server", "client"):
            ready = run_kubectl(args, ["wait", "--namespace", namespace, "--for=condition=Ready", f"pod/{pod}", "--timeout=120s"])
            if ready.returncode != 0:
                raise ValidationError(f"probe Pod {pod} did not become ready: {(ready.stderr or ready.stdout).strip()}")
        baseline = False
        for _ in range(20):
            baseline_attempt = run_kubectl(
                args,
                ["exec", "--namespace", namespace, "client", "--", "wget", "-q", "-T", "2", "-O", "-", "http://server:8080"],
            )
            if baseline_attempt.returncode == 0 and baseline_attempt.stdout.strip() == "ready":
                baseline = True
                break
            time.sleep(1)
        if not baseline:
            raise ValidationError("probe Service baseline failed before policy; DNS, endpoints, or workload readiness is broken")
        apply_object(
            args,
            {
                "apiVersion": "networking.k8s.io/v1",
                "kind": "NetworkPolicy",
                "metadata": {"name": "deny-server", "namespace": namespace, "labels": labels},
                "spec": {"podSelector": {"matchLabels": {"role": "server"}}, "policyTypes": ["Ingress"]},
            },
        )
        consecutive_denials = 0
        for _ in range(12):
            denied_attempt = run_kubectl(
                args,
                ["exec", "--namespace", namespace, "client", "--", "wget", "-q", "-T", "2", "-O", "-", "http://server:8080"],
            )
            if denied_attempt.returncode != 0:
                consecutive_denials += 1
                if consecutive_denials >= 3:
                    denied = True
                    break
            else:
                consecutive_denials = 0
            time.sleep(1)
        if not denied:
            raise ValidationError("NetworkPolicy deny could not be proven: client reached the protected server")
        apply_object(
            args,
            {
                "apiVersion": "networking.k8s.io/v1",
                "kind": "NetworkPolicy",
                "metadata": {"name": "allow-client", "namespace": namespace, "labels": labels},
                "spec": {
                    "podSelector": {"matchLabels": {"role": "server"}},
                    "policyTypes": ["Ingress"],
                    "ingress": [{"from": [{"podSelector": {"matchLabels": {"role": "client"}}}], "ports": [{"protocol": "TCP", "port": 8080}]}],
                },
            },
        )
        for _ in range(20):
            allowed_attempt = run_kubectl(
                args,
                ["exec", "--namespace", namespace, "client", "--", "wget", "-q", "-T", "2", "-O", "-", "http://server:8080"],
            )
            if allowed_attempt.returncode == 0 and allowed_attempt.stdout.strip() == "ready":
                allowed = True
                break
            time.sleep(1)
        if not allowed:
            raise ValidationError("NetworkPolicy allow could not be proven: authorized client did not reach the protected server")
    finally:
        if cleanup_signals_unblocked:
            signal.pthread_sigmask(signal.SIG_BLOCK, cleanup_signals)
        if namespace_created:
            try:
                observed_uid, observed_run_id = namespace_identity(args, namespace)
            except ValidationError as error:
                cleanup_error = f"cannot prove ownership before cleanup: {error}"
            else:
                owned = observed_run_id == args.run_id and namespace_uid is not None and observed_uid == namespace_uid
            if cleanup_error is None and owned:
                deletion = run_kubectl(args, ["delete", "namespace", namespace, "--wait=true", "--timeout=120s"])
                if deletion.returncode != 0:
                    cleanup_error = (
                        f"owned namespace cleanup failed: {(deletion.stderr or deletion.stdout).strip()}; "
                        f"retry: kubectl --kubeconfig {args.kubeconfig} --context {args.context} delete namespace {namespace}"
                    )
            elif cleanup_error is None:
                cleanup_error = (
                    f"refusing cleanup because namespace {namespace} no longer has run ownership "
                    f"label {args.run_id} and UID {namespace_uid}; "
                    "inspect the namespace before any manual deletion"
                )
        restore_signal_handlers(previous_signal_handlers)
        signal.pthread_sigmask(signal.SIG_SETMASK, previous_signal_mask)
    if cleanup_error is not None:
        raise ValidationError(cleanup_error)
    if not denied or not allowed:
        raise ValidationError("NetworkPolicy enforcement smoke did not complete")
    print(
        canonical(
            {
                "schemaVersion": RESULT_CONTRACT,
                "status": "valid",
                "operation": "network-smoke",
                "runId": args.run_id,
                "proof": {
                    "baselineConnectionObserved": True,
                    "deniedConnectionObserved": True,
                    "allowedConnectionObserved": True,
                },
                "cleanup": "deleted",
            }
        ),
        end="",
    )
    return 0


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser(description=__doc__)
    commands = root.add_subparsers(dest="command", required=True)
    for name, handler in (("generate", generate), ("validate", validate_command)):
        command = commands.add_parser(name)
        command.add_argument("--input", type=pathlib.Path, required=True)
        command.add_argument("--provider-profile-bundle", type=pathlib.Path, required=True)
        command.add_argument("--chart", type=pathlib.Path)
        if name == "generate":
            command.add_argument("--output", type=pathlib.Path, required=True)
        command.set_defaults(handler=handler)
    gateway = commands.add_parser("gateway-check")
    gateway.add_argument("--input", type=pathlib.Path, required=True)
    gateway.add_argument("--kubeconfig", type=pathlib.Path, required=True)
    gateway.add_argument("--context", required=True)
    gateway.add_argument("--kubectl", default="kubectl")
    gateway.set_defaults(handler=gateway_check)
    for name, handler in (("network-check", network_check), ("network-smoke", network_smoke)):
        network = commands.add_parser(name)
        network.add_argument("--kubeconfig", type=pathlib.Path, required=True)
        network.add_argument("--context", required=True)
        network.add_argument("--kubectl", default="kubectl")
        if name == "network-smoke":
            network.add_argument("--run-id", required=True)
            network.add_argument("--probe-image", required=True)
        network.set_defaults(handler=handler)
    return root


def main() -> int:
    args = parser().parse_args()
    try:
        return args.handler(args)
    except ValidationError as error:
        print(canonical({"schemaVersion": RESULT_CONTRACT, "status": "invalid", "diagnostics": [{"code": "validation.failed", "severity": "error", "message": str(error)}]}), end="", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
