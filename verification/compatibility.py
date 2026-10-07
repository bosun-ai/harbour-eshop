#!/usr/bin/env python3
"""Compare synthetic scenarios on independent seeded Harbour runtimes.

Uses the real Docker entrypoint and gateway executable. Temporary credentials,
cookie jars and diagnostic pages never leave the temporary directory or stdout.
"""
import argparse
import http.client
import json
import os
from pathlib import Path
import re
import signal
import socket
import ssl
import subprocess
import tempfile
import time
from urllib.parse import urljoin, urlsplit

ROOT = Path(__file__).resolve().parents[1]


def command(*args):
    return subprocess.check_output(args, stderr=subprocess.PIPE).decode().strip()


def certificate(directory, name, san):
    command("openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "2",
            "-subj", f"/CN={name}", "-addext", f"subjectAltName={san}",
            "-addext", "basicConstraints=critical,CA:FALSE",
            "-keyout", str(directory / f"{name}.key"), "-out", str(directory / f"{name}.crt"))


def wait(check, seconds=30):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        try:
            if check():
                return
        except (OSError, ssl.SSLError, http.client.HTTPException):
            pass
        time.sleep(0.1)
    raise AssertionError("bounded readiness wait failed")


class Client:
    def __init__(self, port, trust):
        self.port = port
        self.context = ssl.create_default_context(cafile=str(trust))
        self.cookies = {}
        self.ids = {}

    def request(self, path, method="GET", form=None, follow=True):
        hops = []
        for _ in range(8):
            connection = http.client.HTTPSConnection("localhost", self.port, context=self.context, timeout=5)
            headers = {"Host": "shop.example", "User-Agent": "bootstrap-verification"}
            if self.cookies:
                headers["Cookie"] = "; ".join(f"{key}={value}" for key, value in self.cookies.items())
            if form is not None:
                method = "POST"
                headers["Content-Type"] = "application/x-www-form-urlencoded"
            connection.request(method, path, body=form, headers=headers)
            response = connection.getresponse()
            body = response.read()
            selected = []
            for key, value in response.getheaders():
                key = key.lower()
                if key == "set-cookie":
                    match = re.match(r"([^=]+)=([^;]*)(.*)", value)
                    name, token, attributes = match.groups()
                    # Compare per-instance session continuity/rotation, not random IDs.
                    identity = self.ids.setdefault(token, len(self.ids) + 1)
                    selected.append((key, f"{name}=session-{identity}{attributes}"))
                    if "max-age=0" in attributes.lower():
                        self.cookies.pop(name, None)
                    else:
                        self.cookies[name] = token
                elif key in {"location", "content-type", "content-length", "last-modified", "content-encoding"}:
                    selected.append((key, value))
            if path.startswith("/info"):
                body = normalize_info(body, self.ids)
                selected = [(key, value) for key, value in selected if key != "content-length"]
            hops.append((response.status, sorted(selected), body))
            location = response.getheader("Location")
            connection.close()
            if not follow or not location or response.status not in (301, 302, 303, 307, 308):
                return hops
            destination = urlsplit(urljoin("https://shop.example" + path, location))
            assert destination.hostname == "shop.example", "unexpected redirect authority"
            path = destination.path + ("?" + destination.query if destination.query else "")
            if response.status == 303:
                method, form = "GET", None
        raise AssertionError("redirect loop")


# Only transport fields that this adapter demonstrably changes are normalized.
INFO_FIELDS = {"REMOTE_ADDR", "REMOTE_HOST", "REMOTE_PORT", "SERVER_ADDR", "SERVER_PORT",
               "SSL_CIPHER", "SSL_PROTOCOL", "SSL_CIPHER_USEKEYSIZE", "SSL_CIPHER_ALGKEYSIZE",
               "HTTP_CONNECTION", "HTTP_CONTENT_LENGTH", "CONTENT_LENGTH",
               "HTTP_X_FORWARDED_FOR", "HTTP_X_FORWARDED_PROTO", "HTTP_X_REQUEST_ID"}


def normalize_info(body, identities):
    text = body.decode()
    def cookie(match):
        token = match.group(1)
        assert token in identities, "diagnostic cookie must match observed session"
        return f"SESSID=session-{identities[token]}"
    text = re.sub(r"SESSID=([a-f0-9]{32})", cookie, text)
    for field in INFO_FIELDS:
        text = re.sub(r"<tr><td>" + field + r"</td><td>.*?</td></tr>\r?\n?", "", text)
    return text.encode()


def management(port, path="/readyz"):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    connection.request("GET", path)
    response = connection.getresponse()
    result = response.status, response.read()
    connection.close()
    return result


def write_config(directory, public_port, management_port, host="localhost", **changes):
    text = (ROOT / "gateway/config/all-legacy.toml").read_text()
    replacements = {"public_bind": f'"127.0.0.1:{public_port}"',
                    "management_bind": f'"127.0.0.1:{management_port}"',
                    "legacy_upstream": f'"https://{host}:8002/"',
                    "public_certificate": f'"{directory}/public.crt"',
                    "public_key": f'"{directory}/public.key"',
                    "upstream_trust": f'"{directory}/upstream.crt"'}
    replacements.update(changes)
    for key, value in replacements.items():
        text = re.sub(rf"^{key} = .*", f"{key} = {value}", text, flags=re.M)
    path = directory / f"config-{public_port}.toml"
    path.write_text(text)
    return path


def raw(port, trust, request):
    context = ssl.create_default_context(cafile=str(trust))
    with context.wrap_socket(socket.create_connection(("localhost", port), timeout=5), server_hostname="localhost") as stream:
        stream.sendall(request)
        data = b""
        while True:
            part = stream.recv(65536)
            if not part:
                return data
            data += part


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--legacy-image", required=True)
    parser.add_argument("--gateway", default=str(ROOT / "gateway/target/release/eshop-gateway"))
    args = parser.parse_args()
    names = []
    processes = []
    with tempfile.TemporaryDirectory(prefix="eshop-verification-") as temporary:
        directory = Path(temporary)
        certificate(directory, "upstream", "DNS:localhost")
        certificate(directory, "public", "DNS:localhost")
        logs = open(directory / "gateway.log", "w+")
        try:
            for label, port in [("direct", 18002), ("proxied", 8002)]:
                name = f"eshop-verify-{os.getpid()}-{label}"
                names.append(name)
                command("docker", "create", "--name", name, "-p", f"127.0.0.1:{port}:8002", args.legacy_image)
                command("docker", "cp", str(directory / "upstream.key"), name + ":/app/private.key")
                command("docker", "cp", str(directory / "upstream.crt"), name + ":/app/certificate.crt")
                command("docker", "start", name)
                wait(lambda: Client(port, directory / "upstream.crt").request("/hello")[0][2] == b"Hello!")
            config = write_config(directory, 18443, 18003)
            gateway = subprocess.Popen([args.gateway, "--config", str(config)], stdout=logs, stderr=logs)
            processes.append(gateway)
            wait(lambda: management(18003) == (200, b"ready\n"))
            direct = Client(18002, directory / "upstream.crt")
            proxied = Client(18443, directory / "public.crt")
            corpus = json.loads((ROOT / "verification/corpus.json").read_text())
            for index, scenario in enumerate(corpus):
                request = {key: value for key, value in scenario.items() if key in {"path", "method", "form"}}
                left, right = direct.request(**request), proxied.request(**request)
                assert left == right, f"compatibility mismatch scenario {index}: {scenario['path']} (no bodies logged)"
                if "status" in scenario:
                    assert left[0][0] == scenario["status"], f"wrong status scenario {index}"
                if "contains" in scenario:
                    assert scenario["contains"].encode() in left[-1][2], f"missing expected content scenario {index}"
            cart = direct.request("/app/cart")
            assert cart == proxied.request("/app/cart")
            # Restart both unchanged entrypoints: DBF writes survive, sessions do not.
            for name in names:
                command("docker", "restart", "-t", "2", name)
            wait(lambda: management(18003)[0] == 200)
            wait(lambda: Client(18002, directory / "upstream.crt").request("/hello")[0][0] == 200)
            left, right = direct.request("/app/cart", follow=False), proxied.request("/app/cart", follow=False)
            assert left == right and left[0][0] == 303, "restart must invalidate sessions"
            for client in (direct, proxied):
                client.request("/app/login", form="user=alice&password=newsecret")
            assert direct.request("/app/account") == proxied.request("/app/account")
            assert direct.request("/app/cart") == proxied.request("/app/cart") == cart, "DBF changes must survive"
            # Safe policy differences: unsupported framing is rejected before mutation.
            for request in [b"POST /hello HTTP/1.1\r\nHost: shop.example\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n0\r\n\r\n",
                            b"POST /hello HTTP/1.1\r\nHost: shop.example\r\nContent-Length: 1048577\r\nConnection: close\r\n\r\n"]:
                result = raw(18443, directory / "public.crt", request)
                assert result.startswith((b"HTTP/1.1 400", b"HTTP/1.1 413")), "framing policy not enforced"
            command("docker", "stop", "-t", "2", names[1])
            assert management(18003)[0] == 503
            assert proxied.request("/hello")[0][0] == 502
            assert management(18003, "/livez")[0] == 200
            command("docker", "start", names[1])
            wait(lambda: management(18003)[0] == 200)
            # Routing rollback to current runtime via direct publication, not seed restore.
            started = time.monotonic()
            gateway.send_signal(signal.SIGTERM)
            gateway.wait(timeout=12)
            assert gateway.returncode == 0 and time.monotonic() - started < 12
            rollback = Client(8002, directory / "upstream.crt")
            rollback.request("/app/login", form="user=alice&password=newsecret")
            assert b"Updated" in rollback.request("/app/account")[-1][2]
            assert rollback.request("/app/cart")[-1][2] == cart[-1][2]
            logs.flush()
            logs.seek(0)
            output = logs.read()
            for secret in ["newsecret", "secret", "SESSID", "alice", "raw=", "add=", "del="]:
                assert secret not in output, "gateway logs leaked request data"
            assert '"owner":"legacy"' in output
            print(f"PASS: {len(corpus)} stateful scenarios and redirect hops; cookies, restart persistence/session loss, readiness transitions, framing, shutdown, redaction and ingress rollback")
        finally:
            for process in processes:
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=12)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait()
            for name in names:
                subprocess.run(["docker", "rm", "-f", name], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
            logs.close()


if __name__ == "__main__":
    main()
