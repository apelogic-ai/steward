#!/usr/bin/env python3
from __future__ import annotations

import copy
import importlib.util
import json
import pathlib
import signal
import subprocess
import sys
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
TOOL = ROOT / "scripts" / "steward-platform-preflight.py"
EXAMPLE = ROOT / "config" / "platform-preflight" / "v1" / "examples" / "compact.json"
SEPARATED = ROOT / "config" / "platform-preflight" / "v1" / "examples" / "separated.json"
COMPLETE = ROOT / "config" / "platform-preflight" / "v1" / "examples" / "governed-complete.json"
sys.dont_write_bytecode = True


def load_tool_module():
    specification = importlib.util.spec_from_file_location("steward_platform_preflight", TOOL)
    assert specification is not None and specification.loader is not None
    module = importlib.util.module_from_spec(specification)
    specification.loader.exec_module(module)
    return module


class PlatformPreflightTests(unittest.TestCase):
    def setUp(self) -> None:
        self.input = json.loads(EXAMPLE.read_text(encoding="utf-8"))
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.profile_bundle = pathlib.Path(self.temporary.name) / "provider-profile-bundle" / "v1.2.2"
        self.write_profile_installer(include_bridge_binary=True)

    def write_profile_installer(
        self,
        include_bridge_binary: bool,
        include_all_profiles: bool = True,
        include_delete: bool = True,
        shared_audience: bool = True,
    ) -> None:
        installer = self.profile_bundle / "bin" / "steward-provider-profile"
        installer.parent.mkdir(parents=True, exist_ok=True)
        validated_profiles = (
            "[{'id': 'steward-litellm', 'digest': 'sha256:' + '7' * 64}, "
            "{'id': 'steward-mcp-gw', 'digest': 'sha256:' + '8' * 64}]"
            if include_all_profiles
            else "[{'id': 'steward-litellm', 'digest': 'sha256:' + '7' * 64}]"
        )
        installer.write_text(
            "#!/usr/bin/env python3\n"
            "import json, sys, urllib.parse\n"
            "command = sys.argv[1]\n"
            "inputs_path = sys.argv[sys.argv.index('--inputs') + 1]\n"
            "inputs = json.load(open(inputs_path, encoding='utf-8'))\n"
            "if command == 'render':\n"
            "  profiles = {}\n"
            "  for profile in inputs['profiles']:\n"
            "    origin = urllib.parse.urlsplit(profile['inputs']['gateway-origin'])\n"
            "    binaries = ['/usr/bin/curl']\n"
            f"    if profile['id'] == 'steward-mcp-gw' and {include_bridge_binary!r}:\n"
            "      binaries.append('/usr/local/bin/steward-connections-bridge')\n"
            "    endpoint = {'host': origin.hostname, 'port': origin.port or 443, 'allowed_ips': profile['inputs']['service-cidrs']}\n"
            "    if profile['id'] == 'steward-mcp-gw':\n"
            "      methods = ['GET', 'HEAD', 'OPTIONS', 'POST', 'PUT', 'PATCH']\n"
            f"      if {include_delete!r}: methods.append('DELETE')\n"
            "      endpoint['rules'] = [{'allow': {'method': method, 'path': '**'}} for method in methods]\n"
            "    else:\n"
            "      endpoint['access'] = 'read-write'\n"
            "    audience = profile['inputs']['mint-audience']\n"
            f"    if profile['id'] == 'steward-mcp-gw' and not {shared_audience!r}: audience = 'other-audience'\n"
            "    profiles[profile['id']] = {\n"
            "      'endpoints': [endpoint],\n"
            "      'credentials': [{'token_grant': {'audience': audience}}],\n"
            "      'binaries': binaries,\n"
            "    }\n"
            "  installation = {'schema': 'steward.provider-profile-install-state/v1', 'bundle': {'id': 'steward-runtime-providers', 'version': '1.2.2'}, 'profiles': profiles}\n"
            "  result = {'schemaVersion': 'steward.provider-profile-result/v1', 'operation': 'render', 'status': 'valid'}\n"
            "  print(json.dumps({'result': result, 'installation': installation}, separators=(',', ':')))\n"
            "else:\n"
            "  print(json.dumps({\n"
            "    'schemaVersion': 'steward.provider-profile-result/v1',\n"
            "    'operation': 'validate',\n"
            "    'status': 'valid',\n"
            "    'bundle': {'id': 'steward-runtime-providers', 'version': '1.2.2'},\n"
            f"    'profiles': {validated_profiles}\n"
            "  }, separators=(',', ':')))\n",
            encoding="utf-8",
        )
        installer.chmod(0o755)

    def run_validate(self, value: dict) -> subprocess.CompletedProcess[str]:
        with tempfile.TemporaryDirectory() as temporary:
            input_path = pathlib.Path(temporary) / "input.json"
            input_path.write_text(json.dumps(value), encoding="utf-8")
            return subprocess.run(
                [
                    str(TOOL),
                    "validate",
                    "--input",
                    str(input_path),
                    "--provider-profile-bundle",
                    str(self.profile_bundle),
                ],
                capture_output=True,
                text=True,
                check=False,
            )

    def write_kubectl(self, path: pathlib.Path, gateway: dict) -> None:
        path.write_text(
            "#!/usr/bin/env python3\n"
            "import json, sys\n"
            f"gateway = {gateway!r}\n"
            "resource = sys.argv[sys.argv.index('get') + 1]\n"
            "name = sys.argv[sys.argv.index('get') + 2]\n"
            "namespace = sys.argv[sys.argv.index('--namespace') + 1]\n"
            "if resource.startswith('gateway.'):\n"
            "    print(json.dumps(gateway, separators=(',', ':')))\n"
            "else:\n"
            "    print(f'{namespace}/{name}', end='')\n",
            encoding="utf-8",
        )
        path.chmod(0o755)

    def write_network_kubectl(
        self, path: pathlib.Path, state: pathlib.Path, enabled: bool = True, cleanup_ok: bool = True
    ) -> None:
        agent_args = ["--enable-network-policy=true"] if enabled else []
        daemonset = {
            "apiVersion": "apps/v1",
            "kind": "DaemonSet",
            "metadata": {"name": "aws-node", "namespace": "kube-system"},
            "spec": {"template": {"spec": {"containers": [{"name": "aws-network-policy-agent", "image": "registry.example.test/eks/network-policy-agent:v1", "args": agent_args}]}}},
        }
        path.write_text(
            "#!/usr/bin/env python3\n"
            "import json, pathlib, sys\n"
            f"state = pathlib.Path({str(state)!r})\n"
            f"cleanup_ok = {cleanup_ok!r}\n"
            f"daemonset = {daemonset!r}\n"
            "args = sys.argv[1:]\n"
            "if 'get' in args and 'daemonset' in args:\n"
            "    print(json.dumps(daemonset, separators=(',', ':')))\n"
            "elif 'create' in args:\n"
            "    value = json.load(sys.stdin)\n"
            "    if state.exists() and state.read_text() == 'preexisting':\n"
            "        print('AlreadyExists', file=sys.stderr)\n"
            "        raise SystemExit(1)\n"
            "    state.write_text('created')\n"
            "    print('created')\n"
            "elif 'apply' in args:\n"
            "    value = json.load(sys.stdin)\n"
            "    kind = value.get('kind')\n"
            "    name = value.get('metadata', {}).get('name')\n"
            "    if kind == 'Service' and name == 'server':\n"
            "        state.write_text('baseline')\n"
            "    elif kind == 'NetworkPolicy' and name == 'deny-server':\n"
            "        state.write_text('deny')\n"
            "    elif kind == 'NetworkPolicy' and name == 'allow-client':\n"
            "        state.write_text('allow')\n"
            "    print('applied')\n"
            "elif 'exec' in args:\n"
            "    if state.exists() and state.read_text() in ('baseline', 'allow'):\n"
            "        print('ready')\n"
            "    else:\n"
            "        raise SystemExit(1)\n"
            "elif 'get' in args and 'namespace' in args:\n"
            "    print(json.dumps({'metadata': {'name': 'steward-netpol-smoke-1', 'uid': 'uid-smoke-1', 'labels': {'steward.test/run-id': 'smoke-1'}}}, separators=(',', ':')))\n"
            "elif 'delete' in args:\n"
            "    if not cleanup_ok:\n"
            "        print('delete failed', file=sys.stderr)\n"
            "        raise SystemExit(1)\n"
            "    state.write_text('deleted')\n"
            "else:\n"
            "    print('ok')\n",
            encoding="utf-8",
        )
        path.chmod(0o755)

    def test_valid_compact_input(self) -> None:
        result = self.run_validate(self.input)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["status"], "valid")

    def test_valid_separated_input(self) -> None:
        separated = json.loads(SEPARATED.read_text(encoding="utf-8"))
        result = self.run_validate(separated)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_rejects_browser_organization_id_outside_steward_namespace(self) -> None:
        self.input["browserAuth"]["organizationId"] = "example-org"
        result = self.run_validate(self.input)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(
            "browserAuth.organizationId must use org_ and lowercase ASCII letters, digits, _ or -",
            result.stderr,
        )

    def test_rejects_browser_organization_id_without_a_suffix(self) -> None:
        self.input["browserAuth"]["organizationId"] = "org_"
        result = self.run_validate(self.input)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(
            "browserAuth.organizationId must use org_ and lowercase ASCII letters, digits, _ or -",
            result.stderr,
        )

    def test_examples_use_released_codex_executable(self) -> None:
        separated = json.loads(SEPARATED.read_text(encoding="utf-8"))
        self.assertEqual(self.input["execution"]["binding"]["executable"], "/usr/bin/codex")
        self.assertEqual(separated["execution"]["binding"]["executable"], "/usr/bin/codex")
        self.assertEqual(
            self.input["execution"]["endpoints"]["inference"],
            "https://inference.example.test/v1/responses",
        )
        self.assertEqual(
            separated["execution"]["endpoints"]["inference"],
            "https://inference.example.test/v1/responses",
        )

    def test_bare_chart_defaults_remain_fail_closed(self) -> None:
        defaults = subprocess.run(
            ["helm", "show", "values", str(ROOT / "charts" / "steward")],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(defaults.returncode, 0, defaults.stderr)
        self.assertIn(
            "capabilityCatalog:\n"
            "      schemaVersion: steward.capability-catalog/v2\n"
            "      models: []\n"
            "      tools: []\n"
            "      catalogs: []",
            defaults.stdout,
        )
        self.assertIn("kubeApiCidrs: []", defaults.stdout)
        self.assertIn("postgresCidrs: []", defaults.stdout)

    def test_rejects_top_level_input_outside_schema(self) -> None:
        self.input["ignoredValue"] = "must-not-be-silent"
        result = self.run_validate(self.input)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("input contains unsupported fields", result.stderr)

    def test_rejects_steward_run_without_envelope_digest_capability(self) -> None:
        self.input["stewardRunRelease"]["version"] = "0.6.0"
        result = self.run_validate(self.input)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("stewardRunRelease.version must be 0.7.0 or later", result.stderr)

    def test_rejects_incompatible_steward_run_handoff_schema(self) -> None:
        self.input["stewardRunRelease"]["manifestSchemaVersion"] = 2
        result = self.run_validate(self.input)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("stewardRunRelease.manifestSchemaVersion must equal 3", result.stderr)

    def test_rejects_steward_run_repository_components_rejected_by_runtime(self) -> None:
        for repository in ("./steward-run", "example-org/.."):
            with self.subTest(repository=repository):
                invalid = copy.deepcopy(self.input)
                invalid["stewardRunRelease"]["workflowRepository"] = repository
                result = self.run_validate(invalid)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(
                    "stewardRunRelease.workflowRepository must be owner/repository",
                    result.stderr,
                )

    def test_rejects_placeholder_digest(self) -> None:
        self.input["deploymentLock"]["artifacts"]["images.apiserver"]["target"]["digest"] = "sha256:" + "0" * 64
        result = self.run_validate(self.input)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("all-zero placeholder", result.stderr)

    def test_rejects_chart_coordinate_not_bound_to_mirrored_artifact(self) -> None:
        self.input["deploymentLock"]["artifacts"]["images.apiserver"]["target"]["reference"] = (
            "registry.example.test/team-a/different:wrong-tag"
        )
        result = self.run_validate(self.input)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("reference for apiserver does not match artifacts target", result.stderr)

    def test_rejects_runtime_image_not_bound_to_mirrored_artifact(self) -> None:
        self.input["deploymentLock"]["executionBindingImages"]["codex"] = (
            "registry.example.test/team-a/other-runtime@sha256:" + "9" * 64
        )
        result = self.run_validate(self.input)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("runtime image for codex does not match artifacts target", result.stderr)

    def test_rejects_incomplete_deployment_lock(self) -> None:
        del self.input["deploymentLock"]["release"]
        result = self.run_validate(self.input)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("deploymentLock.release must be an object", result.stderr)

    def test_rejects_deployment_lock_without_requested_platform_field(self) -> None:
        del self.input["deploymentLock"]["requestedPlatform"]
        result = self.run_validate(self.input)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("deploymentLock.requestedPlatform is required", result.stderr)

    def test_rejects_missing_database_ca(self) -> None:
        del self.input["database"]["tls"]["ca"]
        result = self.run_validate(self.input)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("database.tls.ca is required", result.stderr)

    def test_wildcard_covers_exactly_one_label(self) -> None:
        self.input["publicEndpoints"][0]["hostname"] = "service.product.example.test"
        result = self.run_validate(self.input)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not covered", result.stderr)

    def test_explicit_san_covers_nested_hostname(self) -> None:
        self.input["publicEndpoints"][0]["hostname"] = "service.product.example.test"
        self.input["gateway"]["certificateNames"].append("service.product.example.test")
        result = self.run_validate(self.input)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_rejects_secret_body(self) -> None:
        self.input["externalSecrets"]["mint"]["value"] = "not-allowed"
        result = self.run_validate(self.input)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not secret material", result.stderr)

    def test_rejects_incomplete_provider_profile_result(self) -> None:
        self.write_profile_installer(
            include_bridge_binary=True, include_all_profiles=False
        )
        result = self.run_validate(self.input)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("must exactly match requested profiles", result.stderr)

    def test_rejects_connections_origin_not_matching_the_rendered_mcp_profile(self) -> None:
        self.input["execution"]["connectionsBridge"]["mcpGatewayOrigin"] = (
            "https://other-mcp.example.test"
        )
        result = self.run_validate(self.input)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(
            "execution.connectionsBridge.mcpGatewayOrigin host and effective port",
            result.stderr,
        )
        self.assertIn("other-mcp.example.test:443", result.stderr)
        self.assertIn("mcp.example.test:443", result.stderr)

    def test_rejects_wildcard_provider_profile_cidr(self) -> None:
        self.input["providerProfiles"][1]["inputs"]["serviceCidrs"] = ["0.0.0.0/0"]
        result = self.run_validate(self.input)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("overlaps an OpenShell always-blocked range", result.stderr)

    def test_rejects_every_openshell_always_blocked_provider_cidr(self) -> None:
        for cidr in (
            "0.0.0.0/32",
            "127.0.0.0/8",
            "127.1.0.0/16",
            "169.254.0.0/16",
            "::/0",
            "::/128",
            "::1/128",
            "fe80::/10",
            "::ffff:0.0.0.0/128",
            "::ffff:127.0.0.1/128",
            "::ffff:169.254.1.1/128",
        ):
            with self.subTest(cidr=cidr):
                value = copy.deepcopy(self.input)
                value["providerProfiles"][1]["inputs"]["serviceCidrs"] = [cidr]
                result = self.run_validate(value)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("overlaps an OpenShell always-blocked range", result.stderr)

    def test_accepts_in_cluster_http_litellm_management_url(self) -> None:
        self.input["execution"]["endpoints"]["litellm"] = (
            "http://litellm.steward-system.svc:4000"
        )
        result = self.run_validate(self.input)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_rejects_external_or_mismatched_http_litellm_management_url(self) -> None:
        for value in (
            "http://inference.example.test:4000",
            "http://litellm.other-system.svc:4000",
            "http://litellm.steward-system.svc:8080",
        ):
            with self.subTest(value=value):
                candidate = copy.deepcopy(self.input)
                candidate["execution"]["endpoints"]["litellm"] = value
                result = self.run_validate(candidate)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("may use HTTP only for the in-cluster", result.stderr)

    def test_arc_is_optional_when_no_arc_runner_is_used(self) -> None:
        del self.input["arc"]
        del self.input["namespaces"]["arc"]
        result = self.run_validate(self.input)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_arc_configuration_is_all_or_nothing(self) -> None:
        without_namespace = copy.deepcopy(self.input)
        del without_namespace["namespaces"]["arc"]
        result = self.run_validate(without_namespace)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("namespaces.arc is required", result.stderr)

        without_arc = copy.deepcopy(self.input)
        del without_arc["arc"]
        result = self.run_validate(without_arc)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("namespaces.arc must be omitted", result.stderr)

    def test_rejects_mcp_profile_without_connections_bridge_binary(self) -> None:
        self.write_profile_installer(include_bridge_binary=False)
        result = self.run_validate(self.input)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(
            "must include /usr/local/bin/steward-connections-bridge",
            result.stderr,
        )

    def test_rejects_rendered_profile_without_mcp_delete_transport(self) -> None:
        self.write_profile_installer(include_bridge_binary=True, include_delete=False)
        result = self.run_validate(self.input)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("including DELETE", result.stderr)

    def test_rejects_rendered_profile_with_mismatched_mint_audience(self) -> None:
        self.write_profile_installer(include_bridge_binary=True, shared_audience=False)
        result = self.run_validate(self.input)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("audience must equal execution.endpoints.mintAudience", result.stderr)

    def test_generate_is_deterministic(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            base = pathlib.Path(temporary)
            input_path = base / "input.json"
            input_path.write_text(json.dumps(self.input), encoding="utf-8")
            for name in ("first", "second"):
                result = subprocess.run([str(TOOL), "generate", "--input", str(input_path), "--provider-profile-bundle", str(self.profile_bundle), "--output", str(base / name)], capture_output=True, text=True, check=False)
                self.assertEqual(result.returncode, 0, result.stderr)
            for filename in ("steward-values.json", "provider-profile-inputs.json", "namespace-references.json", "diagnostics.json", "flux-values-configmap.yaml", "summary.txt"):
                self.assertEqual((base / "first" / filename).read_bytes(), (base / "second" / filename).read_bytes())
            values = (base / "first" / "steward-values.json").read_text(encoding="utf-8")
            self.assertNotIn("not-allowed", values)
            self.assertIn("steward@sha256:5555", values)
            rendered_values = json.loads(values)
            self.assertTrue(rendered_values["execution"]["enabled"])
            self.assertTrue(rendered_values["connectionsBridge"]["enabled"])
            self.assertEqual(rendered_values["config"]["apiserver"]["executionBindingsMode"], "active")
            profiles = rendered_values["config"]["apiserver"]["executionBindings"]["bindings"][0]["providerProfiles"]
            self.assertEqual(profiles["tools"]["digest"], "sha256:" + "8" * 64)
            self.assertEqual(profiles["inference"]["digest"], "sha256:" + "7" * 64)
            profile_inputs = json.loads((base / "first" / "provider-profile-inputs.json").read_text(encoding="utf-8"))
            self.assertEqual(profile_inputs["bundle"]["version"], "1.2.2")
            self.assertEqual(
                {profile["inputs"]["mint-audience"] for profile in profile_inputs["profiles"]},
                {"steward-mcp"},
            )
            self.assertEqual(rendered_values["config"]["mint"]["audience"], "steward-mcp")

    def test_complete_governed_example_emits_and_renders_all_required_values(self) -> None:
        complete = json.loads(COMPLETE.read_text(encoding="utf-8"))
        with tempfile.TemporaryDirectory() as temporary:
            base = pathlib.Path(temporary)
            input_path = base / "input.json"
            output = base / "rendered"
            input_path.write_text(json.dumps(complete), encoding="utf-8")
            result = subprocess.run(
                [
                    str(TOOL),
                    "generate",
                    "--input",
                    str(input_path),
                    "--provider-profile-bundle",
                    str(self.profile_bundle),
                    "--chart",
                    str(ROOT / "charts" / "steward"),
                    "--output",
                    str(output),
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            values = json.loads((output / "steward-values.json").read_text(encoding="utf-8"))
            self.assertEqual(values["config"]["apiserver"]["capabilityCatalog"], complete["capabilityCatalog"])
            self.assertEqual(
                values["config"]["apiserver"]["stewardRunRelease"],
                complete["stewardRunRelease"],
            )
            self.assertEqual(values["networkPolicy"]["kubeApiCidrs"], complete["networkPolicy"]["kubeApiCidrs"])
            self.assertEqual(values["networkPolicy"]["postgresCidrs"], complete["networkPolicy"]["postgresCidrs"])
            self.assertEqual(values["config"]["apiserver"]["inferenceEndpoint"], "https://inference.example.test/v1/responses")
            self.assertEqual(
                values["web"]["httpRoute"]["backendTls"],
                {
                    "hostname": "steward-apiserver.steward-system.svc.cluster.local",
                    "caConfigMap": {"name": "steward-apiserver-ca", "key": "ca.crt"},
                },
            )
            self.assertEqual(
                values["web"]["httpRoute"]["apiPaths"],
                [
                    {"type": "Exact", "value": "/.well-known/oauth-protected-resource"},
                    {"type": "PathPrefix", "value": "/admin/api"},
                    {"type": "PathPrefix", "value": "/admin/auth"},
                    {"type": "Exact", "value": "/admin/connections/github/callback"},
                    {"type": "PathPrefix", "value": "/admin/operator"},
                    {"type": "PathPrefix", "value": "/app/api"},
                    {"type": "PathPrefix", "value": "/v1"},
                ],
            )
            self.assertEqual(
                values["connectionsBridge"]["mcpGatewayAuthorityContract"],
                "steward.connections.github/v2",
            )
            self.assertEqual(values["connectionsBridge"]["mcpGatewayVersion"], "")

            flux = (output / "flux-values-configmap.yaml").read_text(encoding="utf-8")
            flux_lines = flux.splitlines()
            values_marker = flux_lines.index("  values.json: |")
            flux_values = "\n".join(line.removeprefix("    ") for line in flux_lines[values_marker + 1 :])
            self.assertEqual(json.loads(flux_values), values)
            summary = (output / "summary.txt").read_text(encoding="utf-8")
            for line in (
                "Capability models: 1",
                "Capability tools: 1",
                "Execution bindings: 1",
                "Runtime namespaces: 2",
                "API caller namespaces: 1",
                "Kubernetes API CIDRs: 1",
                "PostgreSQL CIDRs: 1",
            ):
                self.assertIn(line, summary)

            rendered = subprocess.run(
                [
                    "helm",
                    "template",
                    "steward",
                    str(ROOT / "charts" / "steward"),
                    "--namespace",
                    complete["namespaces"]["steward"],
                    "--values",
                    str(output / "steward-values.json"),
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(rendered.returncode, 0, rendered.stderr)
            self.assertIn("capability-catalog.json:", rendered.stdout)
            self.assertIn("gpt-5.4", rendered.stdout)
            self.assertIn("actions_get", rendered.stdout)
            self.assertIn("checksum/capability-catalog:", rendered.stdout)
            self.assertIn("mountPath: /run/capability-catalog", rendered.stdout)
            self.assertIn("cidr: 192.0.2.10/32", rendered.stdout)
            self.assertIn("cidr: 192.0.2.20/32", rendered.stdout)
            self.assertIn("kind: BackendTLSPolicy", rendered.stdout)
            self.assertIn('name: "steward-apiserver-ca"', rendered.stdout)

    def test_rejects_incomplete_gateway_tls_or_unknown_connection_contract(self) -> None:
        del self.input["gateway"]["backendTls"]
        result = self.run_validate(self.input)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("gateway.backendTls must be an object", result.stderr)

        self.input = json.loads(EXAMPLE.read_text(encoding="utf-8"))
        self.input["execution"]["connectionsBridge"]["mcpGatewayAuthorityContract"] = "unknown"
        result = self.run_validate(self.input)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("mcpGatewayAuthorityContract is unsupported", result.stderr)

    def test_rejects_empty_capability_catalog(self) -> None:
        self.input["capabilityCatalog"]["models"] = []
        self.input["capabilityCatalog"]["tools"] = []
        result = self.run_validate(self.input)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("capabilityCatalog must contain at least one model or tool", result.stderr)

    def test_rejects_catalog_missing_category_required_by_binding(self) -> None:
        self.input["capabilityCatalog"]["models"] = []
        result = self.run_validate(self.input)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("must be non-empty when the binding uses inference", result.stderr)

        self.input = json.loads(EXAMPLE.read_text(encoding="utf-8"))
        self.input["capabilityCatalog"]["tools"] = []
        result = self.run_validate(self.input)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("must be non-empty when the binding uses tools", result.stderr)

    def test_allows_catalog_category_omitted_by_binding(self) -> None:
        del self.input["execution"]["binding"]["toolsProfile"]
        self.input["capabilityCatalog"]["tools"] = []
        result = self.run_validate(self.input)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_rejects_malformed_or_duplicate_capability_entry(self) -> None:
        self.input["capabilityCatalog"]["models"] = [
            {"provider": "litellm", "model": "gpt-5.4"},
            {"provider": "litellm", "model": "gpt-5.4"},
        ]
        result = self.run_validate(self.input)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("contains duplicate entry", result.stderr)
        self.input["capabilityCatalog"]["models"] = [{"provider": "litellm"}]
        result = self.run_validate(self.input)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("model must be a non-empty string", result.stderr)

        self.input = json.loads(EXAMPLE.read_text(encoding="utf-8"))
        self.input["capabilityCatalog"]["tools"][0]["accessClass"] = "unknown"
        result = self.run_validate(self.input)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("accessClass must be read, write, or destructive", result.stderr)

        self.input = json.loads(EXAMPLE.read_text(encoding="utf-8"))
        self.input["capabilityCatalog"]["catalogs"][0]["available"] = "yes"
        result = self.run_validate(self.input)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("available must be a boolean", result.stderr)

    def test_rejects_missing_empty_or_malformed_required_network_policy_cidrs(self) -> None:
        del self.input["networkPolicy"]["kubeApiCidrs"]
        result = self.run_validate(self.input)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("networkPolicy.kubeApiCidrs must be a non-empty CIDR array", result.stderr)

        self.input = json.loads(EXAMPLE.read_text(encoding="utf-8"))
        self.input["networkPolicy"]["kubeApiCidrs"] = []
        result = self.run_validate(self.input)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("networkPolicy.kubeApiCidrs must be a non-empty CIDR array", result.stderr)

        self.input = json.loads(EXAMPLE.read_text(encoding="utf-8"))
        del self.input["networkPolicy"]["postgresCidrs"]
        result = self.run_validate(self.input)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("networkPolicy.postgresCidrs must be a non-empty CIDR array", result.stderr)

        self.input = json.loads(EXAMPLE.read_text(encoding="utf-8"))
        self.input["networkPolicy"]["postgresCidrs"] = []
        result = self.run_validate(self.input)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("networkPolicy.postgresCidrs must be a non-empty CIDR array", result.stderr)

        self.input = json.loads(EXAMPLE.read_text(encoding="utf-8"))
        self.input["networkPolicy"]["kubeApiCidrs"] = ["not-a-cidr"]
        result = self.run_validate(self.input)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("is not a valid CIDR", result.stderr)

    def test_namespace_change_updates_generated_references(self) -> None:
        separated = json.loads(SEPARATED.read_text(encoding="utf-8"))
        separated["namespaces"]["gateway"] = "edge-v2"
        separated["gateway"]["parentRef"]["namespace"] = "edge-v2"
        with tempfile.TemporaryDirectory() as temporary:
            base = pathlib.Path(temporary)
            input_path = base / "input.json"
            input_path.write_text(json.dumps(separated), encoding="utf-8")
            result = subprocess.run([str(TOOL), "generate", "--input", str(input_path), "--provider-profile-bundle", str(self.profile_bundle), "--output", str(base / "rendered")], capture_output=True, text=True, check=False)
            self.assertEqual(result.returncode, 0, result.stderr)
            values = json.loads((base / "rendered" / "steward-values.json").read_text(encoding="utf-8"))
            references = json.loads((base / "rendered" / "namespace-references.json").read_text(encoding="utf-8"))
            self.assertEqual(values["web"]["httpRoute"]["parentRefs"][0]["namespace"], "edge-v2")
            self.assertEqual(values["networkPolicy"]["ingressNamespace"], "edge-v2")
            self.assertEqual(references["gatewayParentRef"]["namespace"], "edge-v2")

    def test_stale_namespace_reference_is_rejected(self) -> None:
        self.input["namespaces"]["providers"] = "providers-v2"
        result = self.run_validate(self.input)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("must equal namespaces.providers", result.stderr)

    def test_live_gateway_identity_listener_and_certificate(self) -> None:
        gateway = {
            "apiVersion": "gateway.networking.k8s.io/v1",
            "kind": "Gateway",
            "metadata": {"name": "public-gateway", "namespace": "gateway-system"},
            "spec": {
                "listeners": [
                    {
                        "name": "https",
                        "protocol": "HTTPS",
                        "hostname": "*.example.test",
                        "tls": {"certificateRefs": [{"name": "steward-edge-tls", "kind": "Secret"}]},
                    }
                ]
            },
        }
        with tempfile.TemporaryDirectory() as temporary:
            base = pathlib.Path(temporary)
            input_path = base / "input.json"
            input_path.write_text(json.dumps(self.input), encoding="utf-8")
            kubeconfig = base / "kubeconfig"
            kubeconfig.write_text("test", encoding="utf-8")
            kubectl = base / "kubectl"
            self.write_kubectl(kubectl, gateway)
            result = subprocess.run(
                [str(TOOL), "gateway-check", "--input", str(input_path), "--kubeconfig", str(kubeconfig), "--context", "steward-test", "--kubectl", str(kubectl)],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(json.loads(result.stdout)["operation"], "gateway-check")

    def test_live_gateway_wrong_identity_is_rejected(self) -> None:
        gateway = {
            "apiVersion": "gateway.networking.k8s.io/v1",
            "kind": "Gateway",
            "metadata": {"name": "another-gateway", "namespace": "gateway-system"},
            "spec": {"listeners": []},
        }
        with tempfile.TemporaryDirectory() as temporary:
            base = pathlib.Path(temporary)
            input_path = base / "input.json"
            input_path.write_text(json.dumps(self.input), encoding="utf-8")
            kubeconfig = base / "kubeconfig"
            kubeconfig.write_text("test", encoding="utf-8")
            kubectl = base / "kubectl"
            self.write_kubectl(kubectl, gateway)
            result = subprocess.run(
                [str(TOOL), "gateway-check", "--input", str(input_path), "--kubeconfig", str(kubeconfig), "--context", "steward-test", "--kubectl", str(kubectl)],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("does not equal configured parent", result.stderr)

    def test_live_gateway_rejects_certificate_reference_in_another_namespace(self) -> None:
        gateway = {
            "apiVersion": "gateway.networking.k8s.io/v1",
            "kind": "Gateway",
            "metadata": {"name": "public-gateway", "namespace": "gateway-system"},
            "spec": {
                "listeners": [
                    {
                        "name": "https",
                        "protocol": "HTTPS",
                        "hostname": "*.example.test",
                        "tls": {
                            "certificateRefs": [
                                {
                                    "name": "steward-edge-tls",
                                    "kind": "Secret",
                                    "namespace": "another-system",
                                }
                            ]
                        },
                    }
                ]
            },
        }
        with tempfile.TemporaryDirectory() as temporary:
            base = pathlib.Path(temporary)
            input_path = base / "input.json"
            input_path.write_text(json.dumps(self.input), encoding="utf-8")
            kubeconfig = base / "kubeconfig"
            kubeconfig.write_text("test", encoding="utf-8")
            kubectl = base / "kubectl"
            self.write_kubectl(kubectl, gateway)
            result = subprocess.run(
                [str(TOOL), "gateway-check", "--input", str(input_path), "--kubeconfig", str(kubeconfig), "--context", "steward-test", "--kubectl", str(kubectl)],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("does not reference TLS Secret", result.stderr)

    def test_network_preflight_requires_enabled_eks_agent(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            base = pathlib.Path(temporary)
            kubeconfig = base / "kubeconfig"
            kubeconfig.write_text("test", encoding="utf-8")
            kubectl = base / "kubectl"
            self.write_network_kubectl(kubectl, base / "state", enabled=False)
            result = subprocess.run(
                [str(TOOL), "network-check", "--kubeconfig", str(kubeconfig), "--context", "steward-test", "--kubectl", str(kubectl)],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("not observable", result.stderr)

    def test_network_smoke_installs_interrupt_cleanup_handlers(self) -> None:
        module = load_tool_module()
        previous = module.install_cleanup_signal_handlers()
        try:
            for signal_number in (signal.SIGINT, signal.SIGTERM):
                handler = signal.getsignal(signal_number)
                self.assertTrue(callable(handler))
                with self.assertRaises(module.ValidationError):
                    handler(signal_number, None)
        finally:
            module.restore_signal_handlers(previous)

    def test_network_smoke_proves_deny_allow_and_cleanup(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            base = pathlib.Path(temporary)
            kubeconfig = base / "kubeconfig"
            kubeconfig.write_text("test", encoding="utf-8")
            state = base / "state"
            kubectl = base / "kubectl"
            self.write_network_kubectl(kubectl, state)
            image = "registry.example.test/team-a/network-probe@sha256:" + "6" * 64
            result = subprocess.run(
                [str(TOOL), "network-smoke", "--kubeconfig", str(kubeconfig), "--context", "steward-test", "--kubectl", str(kubectl), "--run-id", "smoke-1", "--probe-image", image],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            proof = json.loads(result.stdout)["proof"]
            self.assertTrue(proof["baselineConnectionObserved"])
            self.assertTrue(proof["deniedConnectionObserved"])
            self.assertTrue(proof["allowedConnectionObserved"])
            self.assertEqual(state.read_text(encoding="utf-8"), "deleted")

    def test_network_smoke_fails_when_owned_cleanup_fails(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            base = pathlib.Path(temporary)
            kubeconfig = base / "kubeconfig"
            kubeconfig.write_text("test", encoding="utf-8")
            kubectl = base / "kubectl"
            self.write_network_kubectl(kubectl, base / "state", cleanup_ok=False)
            image = "registry.example.test/team-a/network-probe@sha256:" + "6" * 64
            result = subprocess.run(
                [str(TOOL), "network-smoke", "--kubeconfig", str(kubeconfig), "--context", "steward-test", "--kubectl", str(kubectl), "--run-id", "smoke-1", "--probe-image", image],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("owned namespace cleanup failed", result.stderr)
            self.assertIn("delete namespace steward-netpol-smoke-1", result.stderr)

    def test_network_smoke_never_adopts_or_deletes_a_preexisting_namespace(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            base = pathlib.Path(temporary)
            kubeconfig = base / "kubeconfig"
            kubeconfig.write_text("test", encoding="utf-8")
            state = base / "state"
            state.write_text("preexisting", encoding="utf-8")
            kubectl = base / "kubectl"
            self.write_network_kubectl(kubectl, state)
            image = "registry.example.test/team-a/network-probe@sha256:" + "6" * 64
            result = subprocess.run(
                [str(TOOL), "network-smoke", "--kubeconfig", str(kubeconfig), "--context", "steward-test", "--kubectl", str(kubectl), "--run-id", "smoke-1", "--probe-image", image],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("kubectl create failed", result.stderr)
            self.assertEqual(state.read_text(encoding="utf-8"), "preexisting")


if __name__ == "__main__":
    unittest.main()
