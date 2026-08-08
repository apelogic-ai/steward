#!/usr/bin/env python3
"""Contract-compatible local workload exchange used only by integration tests."""

import argparse
import base64
import hmac
import http.server
import json
import pathlib
import ssl
import subprocess
import time


def base64url(value: bytes) -> str:
    return base64.urlsafe_b64encode(value).rstrip(b"=").decode("ascii")


class ExchangeHandler(http.server.BaseHTTPRequestHandler):
    server_version = "steward-test-workload-exchange"

    def do_POST(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler contract
        if self.path != "/v1/workload/exchange" or self.headers.get("Content-Length", "0") != "0":
            self._json(400, {"error": "invalid_request"})
            return
        expected = pathlib.Path(self.server.source_file).read_text(encoding="utf-8").strip()
        supplied = self.headers.get("Authorization", "")
        if not hmac.compare_digest(supplied, f"Bearer {expected}"):
            self._json(401, {"error": "invalid_token"})
            return
        if self.server.access_file:
            access_token = pathlib.Path(self.server.access_file).read_text(encoding="utf-8").strip()
        else:
            issued_at = int(time.time())
            header = base64url(b'{"alg":"RS256","kid":"steward-test","typ":"JWT"}')
            payload = base64url(
                json.dumps(
                    {
                        "iss": self.server.issuer,
                        "sub": "adapter-test",
                        "preferred_username": "alice",
                        "aud": "openshell-api",
                        "roles": ["openshell-admin", "openshell-user"],
                        "iat": issued_at,
                        "exp": issued_at + 120,
                    },
                    separators=(",", ":"),
                ).encode("utf-8")
            )
            signing_input = f"{header}.{payload}"
            signature = subprocess.run(
                ["openssl", "dgst", "-sha256", "-sign", self.server.signing_key],
                input=signing_input.encode("ascii"),
                check=True,
                capture_output=True,
            ).stdout
            access_token = f"{signing_input}.{base64url(signature)}"
        self._json(
            200,
            {
                "access_token": access_token,
                "token_type": "Bearer",
                "expires_in": 120,
            },
            no_store=True,
        )

    def _json(self, status: int, payload: dict[str, object], no_store: bool = False) -> None:
        body = json.dumps(payload, separators=(",", ":")).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        if no_store:
            self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, format: str, *args: object) -> None:
        return


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--certificate", required=True)
    parser.add_argument("--private-key", required=True)
    parser.add_argument("--source-file", required=True)
    parser.add_argument("--access-file")
    parser.add_argument("--issuer")
    parser.add_argument("--signing-key")
    parser.add_argument("--port-file")
    parser.add_argument("--port", type=int, default=0)
    args = parser.parse_args()
    if bool(args.access_file) == bool(args.issuer and args.signing_key):
        parser.error("provide either --access-file or both --issuer and --signing-key")

    server = http.server.ThreadingHTTPServer(("0.0.0.0", args.port), ExchangeHandler)
    server.source_file = args.source_file
    server.access_file = args.access_file
    server.issuer = args.issuer
    server.signing_key = args.signing_key
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(args.certificate, args.private_key)
    server.socket = context.wrap_socket(server.socket, server_side=True)
    if args.port_file:
        pathlib.Path(args.port_file).write_text(str(server.server_port), encoding="utf-8")
    print(f"LISTENING {server.server_port}", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
