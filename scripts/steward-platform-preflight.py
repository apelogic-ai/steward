#!/usr/bin/env python3
"""Generate and validate deterministic Steward deployment inputs."""

from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import re
import shutil
import subprocess
import sys
import tempfile
from typing import Any


CONTRACT = "steward.platform-input/v1"
RESULT_CONTRACT = "steward.platform-preflight-result/v1"
SHA256 = re.compile(r"^sha256:[0-9a-f]{64}$")
DNS_NAME = re.compile(r"^(?=.{1,253}$)(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$")
K8S_NAME = re.compile(r"^[a-z0-9](?:[a-z0-9.-]{0,251}[a-z0-9])?$")
REQUIRED_NAMESPACES = ("steward", "runtime", "providers", "gateway", "arc", "databaseTls")
IMAGE_COMPONENTS = ("apiserver", "controller", "mint", "web", "bridge")


class ValidationError(Exception):
    pass


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


def validate_name(value: str, path: str) -> None:
    if not K8S_NAME.fullmatch(value):
        raise ValidationError(f"{path} is not a valid Kubernetes name: {value}")


def validate_digest(value: str, path: str) -> None:
    if not SHA256.fullmatch(value):
        raise ValidationError(f"{path} must be an immutable sha256 digest")
    if value == "sha256:" + ("0" * 64):
        raise ValidationError(f"{path} is an all-zero placeholder digest")


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

    namespaces = require_object(data, "namespaces", "input")
    for key in REQUIRED_NAMESPACES:
        validate_name(require_string(namespaces, key, "namespaces"), f"namespaces.{key}")

    lock = require_object(data, "deploymentLock", "input")
    if lock.get("schemaVersion") != "steward.deployment-lock/v1":
        raise ValidationError("deploymentLock.schemaVersion must equal steward.deployment-lock/v1")
    artifacts = require_object(lock, "artifacts", "deploymentLock")
    chart_lock = require_object(lock, "chartValues", "deploymentLock")
    images = require_object(chart_lock, "images", "deploymentLock.chartValues")
    repository = require_string(images, "repository", "deploymentLock.chartValues.images")
    if "@" in repository or repository.endswith(":latest"):
        raise ValidationError("deploymentLock.chartValues.images.repository must not contain a tag or digest")
    for component in IMAGE_COMPONENTS:
        artifact = require_object(artifacts, f"images.{component}", "deploymentLock.artifacts")
        target = require_object(artifact, "target", f"deploymentLock.artifacts.images.{component}")
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
    bridge_values = require_object(chart_lock, "connectionsBridge", "deploymentLock.chartValues")
    bridge_image = require_string(bridge_values, "image", "deploymentLock.chartValues.connectionsBridge")
    bridge_target = require_object(artifacts["images.bridge"], "target", "deploymentLock.artifacts.images.bridge")
    if bridge_image != f"{bridge_target['reference'].rsplit(':', 1)[0]}@{bridge_target['digest']}":
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
    parent = require_object(gateway, "parentRef", "gateway")
    for key in ("name", "namespace", "sectionName"):
        validate_name(require_string(parent, key, "gateway.parentRef"), f"gateway.parentRef.{key}")
    if parent["namespace"] != namespaces["gateway"]:
        raise ValidationError("gateway.parentRef.namespace must equal namespaces.gateway")

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
        validate_name(require_string(profile, "name", f"providerProfiles[{index}]"), f"providerProfiles[{index}].name")
        namespace = require_string(profile, "namespace", f"providerProfiles[{index}]")
        if namespace != namespaces["providers"]:
            raise ValidationError(f"providerProfiles[{index}].namespace must equal namespaces.providers")
        inputs = require_object(profile, "inputs", f"providerProfiles[{index}]")
        validate_digest(require_string(profile, "digest", f"providerProfiles[{index}]"), f"providerProfiles[{index}].digest")
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
    require_string(bridge, "mcpGatewayVersion", "execution.connectionsBridge")
    binding = require_object(execution, "binding", "execution")
    for key in ("agentRef", "displayName", "adapter", "executable", "expectedVersion", "runtimeName"):
        require_string(binding, key, "execution.binding")
    runtime_image = binding_images.get(binding["runtimeName"])
    if not isinstance(runtime_image, str) or not re.fullmatch(r"[^\s@]+@sha256:[0-9a-f]{64}", runtime_image):
        raise ValidationError("execution.binding.runtimeName must select one immutable deploymentLock.executionBindingImages entry")
    profile_ids = {profile["name"] for profile in profiles}
    for key in ("toolsProfile", "inferenceProfile"):
        selected = require_string(binding, key, "execution.binding")
        if selected not in profile_ids:
            raise ValidationError(f"execution.binding.{key} must select one providerProfiles name")

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


def chart_values(data: dict[str, Any]) -> dict[str, Any]:
    lock = data["deploymentLock"]
    endpoint = steward_endpoint(data)
    parent = data["gateway"]["parentRef"]
    database = data["database"]
    external = data["externalSecrets"]
    chart_lock = lock["chartValues"]
    images = chart_lock["images"]
    execution = data["execution"]
    endpoints = execution["endpoints"]
    binding = execution["binding"]
    profiles = {profile["name"]: profile for profile in data["providerProfiles"]}
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
            "mcpGatewayVersion": execution["connectionsBridge"]["mcpGatewayVersion"],
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
                                "tools": {"id": binding["toolsProfile"], "digest": profiles[binding["toolsProfile"]]["digest"]},
                                "inference": {"id": binding["inferenceProfile"], "digest": profiles[binding["inferenceProfile"]]["digest"]},
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
            "ingressNamespace": data["namespaces"]["gateway"],
            "arcNamespace": data["namespaces"]["arc"],
            "apiserverIngressNamespaces": [data["namespaces"]["arc"]],
            "browserAuthEgressCidrs": data["browserAuth"]["egressCidrs"],
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


def build_result(data: dict[str, Any], diagnostics: list[dict[str, str]], values: dict[str, Any]) -> dict[str, Any]:
    digest = "sha256:" + hashlib.sha256(canonical(values).encode()).hexdigest()
    return {
        "schemaVersion": RESULT_CONTRACT,
        "status": "valid",
        "inputContract": CONTRACT,
        "valuesDigest": digest,
        "namespaces": data["namespaces"],
        "gatewayParentRef": data["gateway"]["parentRef"],
        "providerProfiles": data["providerProfiles"],
        "arcControllerServiceAccount": data["arc"]["controllerServiceAccount"],
        "diagnostics": diagnostics,
    }


def generate(args: argparse.Namespace) -> int:
    data = read_json(args.input)
    diagnostics = validate_input(data)
    validate_browser(data)
    values = chart_values(data)
    args.output.mkdir(parents=True, exist_ok=False)
    values_path = args.output / "steward-values.json"
    values_path.write_text(canonical(values), encoding="utf-8")
    (args.output / "provider-profile-inputs.json").write_text(canonical(provider_profile_inputs(data)), encoding="utf-8")
    if args.chart is not None:
        run_helm(args.chart, data["namespaces"]["steward"], values_path)
        diagnostics.append({"code": "chart.valid", "severity": "info", "message": "generated values pass Helm lint and template"})
    result = build_result(data, diagnostics, values)
    (args.output / "diagnostics.json").write_text(canonical(result), encoding="utf-8")
    (args.output / "flux-values-configmap.yaml").write_text(flux_configmap(data["namespaces"]["steward"], values, result["valuesDigest"]), encoding="utf-8")
    summary = (
        "Steward platform preflight: valid\n"
        f"values digest: {result['valuesDigest']}\n"
        f"Steward namespace: {data['namespaces']['steward']}\n"
        f"runtime namespace: {data['namespaces']['runtime']}\n"
        f"public endpoint: {steward_endpoint(data)['hostname']}\n"
        f"Gateway: {data['gateway']['parentRef']['namespace']}/{data['gateway']['parentRef']['name']}\n"
        "Secret bodies emitted: no\n"
    )
    (args.output / "summary.txt").write_text(summary, encoding="utf-8")
    print(canonical(result), end="")
    return 0


def validate_command(args: argparse.Namespace) -> int:
    data = read_json(args.input)
    diagnostics = validate_input(data)
    validate_browser(data)
    values = chart_values(data)
    with tempfile.TemporaryDirectory(prefix="steward-platform-preflight-") as temporary:
        values_path = pathlib.Path(temporary) / "values.json"
        values_path.write_text(canonical(values), encoding="utf-8")
        if args.chart is not None:
            run_helm(args.chart, data["namespaces"]["steward"], values_path)
            diagnostics.append({"code": "chart.valid", "severity": "info", "message": "generated values pass Helm lint and template"})
    print(canonical(build_result(data, diagnostics, values)), end="")
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


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser(description=__doc__)
    commands = root.add_subparsers(dest="command", required=True)
    for name, handler in (("generate", generate), ("validate", validate_command)):
        command = commands.add_parser(name)
        command.add_argument("--input", type=pathlib.Path, required=True)
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
