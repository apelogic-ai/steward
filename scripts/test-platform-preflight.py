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


class PlatformPreflightTests(unittest.TestCase):
    def setUp(self) -> None:
        self.input = json.loads(EXAMPLE.read_text(encoding="utf-8"))

    def run_validate(self, value: dict) -> subprocess.CompletedProcess[str]:
        with tempfile.TemporaryDirectory() as temporary:
            input_path = pathlib.Path(temporary) / "input.json"
            input_path.write_text(json.dumps(value), encoding="utf-8")
            return subprocess.run([str(TOOL), "validate", "--input", str(input_path)], capture_output=True, text=True, check=False)

    def test_valid_compact_input(self) -> None:
        result = self.run_validate(self.input)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["status"], "valid")

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
            for filename in ("steward-values.json", "diagnostics.json", "flux-values-configmap.yaml", "summary.txt"):
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


if __name__ == "__main__":
    unittest.main()
