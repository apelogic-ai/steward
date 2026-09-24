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
        self.profile_bundle = pathlib.Path(self.temporary.name) / "provider-profile-bundle" / "v1.2.0"
        installer = self.profile_bundle / "bin" / "steward-provider-profile"
        installer.parent.mkdir(parents=True)
        installer.write_text(
            "#!/usr/bin/env python3\n"
            "import json\n"
            "print(json.dumps({\n"
            "  'schemaVersion': 'steward.provider-profile-result/v1',\n"
            "  'operation': 'validate',\n"
            "  'status': 'valid',\n"
            "  'bundle': {'id': 'steward-runtime-providers', 'version': '1.2.0'},\n"
            "  'profiles': [\n"
            "    {'id': 'steward-litellm', 'digest': 'sha256:' + '7' * 64},\n"
            "    {'id': 'steward-mcp-gw', 'digest': 'sha256:' + '8' * 64}\n"
            "  ]\n"
            "}, separators=(',', ':')))\n",
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

    def test_examples_use_released_codex_executable(self) -> None:
        separated = json.loads(SEPARATED.read_text(encoding="utf-8"))
        self.assertEqual(self.input["execution"]["binding"]["executable"], "/usr/bin/codex")
        self.assertEqual(separated["execution"]["binding"]["executable"], "/usr/bin/codex")

    def test_rejects_top_level_input_outside_schema(self) -> None:
        self.input["ignoredValue"] = "must-not-be-silent"
        result = self.run_validate(self.input)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("input contains unsupported fields", result.stderr)

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
        installer = self.profile_bundle / "bin" / "steward-provider-profile"
        installer.write_text(
            "#!/usr/bin/env python3\n"
            "import json\n"
            "print(json.dumps({'schemaVersion':'steward.provider-profile-result/v1','operation':'validate','status':'valid','bundle':{'id':'steward-runtime-providers','version':'1.2.0'},'profiles':[{'id':'steward-litellm','digest':'sha256:' + '7' * 64}]}))\n",
            encoding="utf-8",
        )
        installer.chmod(0o755)
        result = self.run_validate(self.input)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("must exactly match requested profiles", result.stderr)

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
            self.assertEqual(profile_inputs["bundle"]["version"], "1.2.0")

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
