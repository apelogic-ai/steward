#!/usr/bin/env python3
from __future__ import annotations

import copy
import json
import pathlib
import subprocess
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
TOOL = ROOT / "scripts" / "steward-platform-preflight.py"
EXAMPLE = ROOT / "config" / "platform-preflight" / "v1" / "examples" / "compact.json"
SEPARATED = ROOT / "config" / "platform-preflight" / "v1" / "examples" / "separated.json"


class PlatformPreflightTests(unittest.TestCase):
    def setUp(self) -> None:
        self.input = json.loads(EXAMPLE.read_text(encoding="utf-8"))

    def run_validate(self, value: dict) -> subprocess.CompletedProcess[str]:
        with tempfile.TemporaryDirectory() as temporary:
            input_path = pathlib.Path(temporary) / "input.json"
            input_path.write_text(json.dumps(value), encoding="utf-8")
            return subprocess.run([str(TOOL), "validate", "--input", str(input_path)], capture_output=True, text=True, check=False)

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

    def test_valid_compact_input(self) -> None:
        result = self.run_validate(self.input)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["status"], "valid")

    def test_valid_separated_input(self) -> None:
        separated = json.loads(SEPARATED.read_text(encoding="utf-8"))
        result = self.run_validate(separated)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_rejects_placeholder_digest(self) -> None:
        self.input["deploymentLock"]["artifacts"]["images.apiserver"]["target"]["digest"] = "sha256:" + "0" * 64
        result = self.run_validate(self.input)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("all-zero placeholder", result.stderr)

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

    def test_generate_is_deterministic(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            base = pathlib.Path(temporary)
            input_path = base / "input.json"
            input_path.write_text(json.dumps(self.input), encoding="utf-8")
            for name in ("first", "second"):
                result = subprocess.run([str(TOOL), "generate", "--input", str(input_path), "--output", str(base / name)], capture_output=True, text=True, check=False)
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
            result = subprocess.run([str(TOOL), "generate", "--input", str(input_path), "--output", str(base / "rendered")], capture_output=True, text=True, check=False)
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


if __name__ == "__main__":
    unittest.main()
