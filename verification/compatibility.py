"""Disposable verified-TLS black-box corpus; never print captured application traffic."""
import http.client
import json
import os
from pathlib import Path
import re
import socket
import ssl
import subprocess
import time
import urllib.parse
import uuid

TMP = Path(os.environ["CHECK_TMP"])
PREFIX = "eshop-check-" + uuid.uuid4().hex[:10]
LEGACY = os.environ["LEGACY_IMAGE"]
GATEWAY = os.environ["GATEWAY_IMAGE"]
NETWORK = PREFIX + "-net"


def command(*args, check=True):
    return subprocess.run(args, check=check, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=120).stdout.decode().strip()


def docker(*args, check=True):
    return command("docker", *args, check=check)


def resource(kind, name):
    with (TMP / "resources").open("a") as stream:
        stream.write(f"{kind} {name}\n")


def certificate(name, san):
    command("openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "2",
            "-subj", f"/CN={san}", "-addext", f"subjectAltName=DNS:{san}",
            "-addext", "basicConstraints=critical,CA:FALSE",
            "-keyout", str(TMP / (name + ".key")), "-out", str(TMP / (name + ".crt")))
    (TMP / (name + ".key")).chmod(0o644)  # Disposable keys readable by image's non-root user.


def create_legacy(name, alias, volume, published=False):
    resource("container", name)
    args = ["create", "--name", name, "--network", NETWORK, "--network-alias", alias,
            "-v", volume + ":/app"]
    if published:
        args += ["-p", "127.0.0.1::8002"]
    docker(*args, LEGACY)
    cert = "public" if published else "private"
    docker("cp", str(TMP / (cert + ".crt")), name + ":/app/certificate.crt")
    docker("cp", str(TMP / (cert + ".key")), name + ":/app/private.key")
    docker("start", name)


def create_gateway(name, upstream="https://legacy:8002", ca="private", extra=None):
    resource("container", name)
    args = ["create", "--name", name, "--network", NETWORK, "-p", "127.0.0.1::8002"]
    env = dict(LEGACY_UPSTREAM_URL=upstream, GATEWAY_TLS_CERT_FILE="/tmp/public.crt",
               GATEWAY_TLS_KEY_FILE="/tmp/public.key", LEGACY_TLS_CA_FILE="/tmp/trust.crt",
               GATEWAY_CONNECT_SECONDS="2", GATEWAY_EXCHANGE_SECONDS="3", GATEWAY_DRAIN_SECONDS="2")
    env.update(extra or {})
    for key, value in env.items():
        args += ["-e", f"{key}={value}"]
    docker(*args, GATEWAY)
    for source, target in [("public.crt", "public.crt"), ("public.key", "public.key"), (ca + ".crt", "trust.crt")]:
        docker("cp", str(TMP / source), name + ":/tmp/" + target)
    docker("start", name)


def port(name):
    return int(docker("port", name, "8002/tcp").rsplit(":", 1)[1])


class Browser:
    def __init__(self, name):
        self.name = name
        self.port = port(name)
        self.cookies = {}

    def request(self, path, method="GET", form=None, chunked=False):
        self.port = port(self.name)
        context = ssl.create_default_context(cafile=str(TMP / "public.crt"))
        conn = http.client.HTTPSConnection("localhost", self.port, context=context, timeout=8)
        headers = {"Host": "browser.example", "Cookie": "; ".join(f"{k}={v}" for k, v in self.cookies.items())}
        data = urllib.parse.urlencode(form).encode() if form is not None else None
        if data is not None:
            headers["Content-Type"] = "application/x-www-form-urlencoded"
        conn.request(method, path, body=iter([data]) if chunked else data, headers=headers, encode_chunked=chunked)
        response = conn.getresponse()
        body = response.read()
        fields = response.getheaders()
        for key, value in fields:
            if key.lower() == "set-cookie":
                cookie = value.split(";", 1)[0].split("=", 1)
                self.cookies[cookie[0]] = cookie[1]
        conn.close()
        return response.status, fields, body


def stable(result):
    status, fields, body = result
    normalized = []
    for key, value in fields:
        key = key.lower()
        if key in {"date", "connection", "transfer-encoding"}:
            continue
        if key == "set-cookie":
            value = re.sub(r"(SESSID=)[^;]*", r"\1<session>", value)
        normalized.append((key, value))
    return status, sorted(normalized), body


def wait_ready(name):
    for _ in range(40):
        result = subprocess.run(["docker", "exec", name, "eshop-gateway", "probe", "ready"], capture_output=True)
        if result.returncode == 0:
            return
        time.sleep(.25)
    raise AssertionError("verified readiness failed")


def wait_browser(browser):
    for _ in range(40):
        try:
            assert browser.request("/hello")[2] == b"Hello!"
            return
        except (OSError, http.client.HTTPException):
            time.sleep(.25)
    raise AssertionError("legacy listener unavailable")


def pair(left, right, path, method="GET", form=None, chunked=False):
    direct = left.request(path, method, form, chunked)
    proxied = right.request(path, method, form, chunked)
    assert stable(direct) == stable(proxied), f"parity failed: {method} {path.split('?')[0]}"
    return proxied


def main():
    certificate("public", "localhost")
    certificate("private", "legacy")
    certificate("wrong", "wrong")
    command("openssl", "x509", "-in", str(TMP / "private.crt"), "-signkey", str(TMP / "private.key"),
            "-days", "-1", "-out", str(TMP / "expired.crt"))
    docker("network", "create", NETWORK)
    (TMP / "network").write_text(NETWORK)
    for suffix in ["direct", "private"]:
        resource("volume", PREFIX + "-" + suffix)
        docker("volume", "create", PREFIX + "-" + suffix)
    direct, legacy, gateway = [PREFIX + "-" + name for name in ["direct", "legacy", "gateway"]]
    create_legacy(direct, "direct", PREFIX + "-direct", True)
    create_legacy(legacy, "legacy", PREFIX + "-private")
    create_gateway(gateway)
    wait_ready(gateway)
    assert json.loads(docker("inspect", legacy))[0]["HostConfig"]["PortBindings"] == {}
    inspect = json.loads(docker("inspect", gateway))[0]
    assert not inspect["Mounts"], "gateway must not mount application state"
    print("Images:", docker("image", "inspect", "--format", "{{.Id}}", LEGACY),
          docker("image", "inspect", "--format", "{{.Id}}", GATEWAY))
    left, right = Browser(direct), Browser(gateway)
    wait_browser(left)
    for path in ["/hello", "/", "/app/login", "/files/main.css", "/unknown", "/files/", "/app/login?err&x=%2F%26", "/app/main"]:
        pair(left, right, path)
    # Only named connection/peer/TLS identities may differ, not arbitrary HTML.
    allowances = {"REMOTE_ADDR", "REMOTE_HOST", "REMOTE_PORT", "SERVER_ADDR",
                  "SSL_CIPHER", "SSL_PROTOCOL", "SSL_CIPHER_USEKEYSIZE", "SSL_CIPHER_ALGKEYSIZE",
                  "SSL_SERVER_I_DN", "SSL_SERVER_S_DN", "HTTP_CONNECTION"}
    def info(browser):
        result = browser.request("/info?encoded=%2F%26")
        text = result[2].decode()
        text = re.sub(r"(SESSID=)[0-9a-f]+", r"\1<session>", text)
        rows = re.findall(r"<tr><td>([^<]+)</td><td>(.*?)</td></tr>", text)
        assert dict(rows)["HTTP_HOST"] == "browser.example"
        for key, value in rows:
            if key in allowances:
                text = text.replace(f"<td>{key}</td><td>{value}</td>", f"<td>{key}</td><td><connection></td>")
        return text
    direct_info, proxy_info = info(left), info(right)
    if direct_info != proxy_info:
        first = dict(re.findall(r"<tr><td>([^<]+)</td><td>(.*?)</td></tr>", direct_info))
        second = dict(re.findall(r"<tr><td>([^<]+)</td><td>(.*?)</td></tr>", proxy_info))
        raise AssertionError("structural info fields: " + ", ".join(key for key in first.keys() | second.keys() if first.get(key) != second.get(key)))
    for method in ["POST", "PUT", "DELETE", "OPTIONS"]:
        pair(left, right, "/hello", method)
    pair(left, right, "/app/login", "POST", {"user": "chunk-user", "password": "chunk-secret"}, True)
    pair(left, right, "/app/register")
    for form in [dict(user="tester", name="", password1="test-secret", password2="test-secret"),
                 dict(user="tester", name="Test", password1="test-secret", password2="mismatch")]:
        result = pair(left, right, "/app/register", "POST", form)
        pair(left, right, "/app/register" + dict((key.lower(), value) for key, value in result[1])["location"])
    pair(left, right, "/app/register", "POST", dict(user="tester", name="Test", password1="test-secret", password2="test-secret"))
    for path in ["/app/main", "/app/account", "/app/shopping", "/app/shopping?_pos=10"]:
        pair(left, right, path)
    pair(left, right, "/app/account/edit", "POST", dict(name="", password1="", password2=""))
    pair(left, right, "/app/account/edit?err=1")
    pair(left, right, "/app/account/edit", "POST", dict(name="Edited", password1="new-secret", password2="new-secret"))
    pair(left, right, "/app/account")
    for _ in range(2):
        pair(left, right, "/app/shopping?add=0001")
    cart = pair(left, right, "/app/cart")
    assert b"53.34" in cart[2]
    pair(left, right, "/app/logout")
    pair(left, right, "/app/main")
    pair(left, right, "/app/login", "POST", dict(user="tester", password="wrong-secret"))
    pair(left, right, "/app/login?err")
    pair(left, right, "/app/login", "POST", dict(user="tester", password="new-secret"))
    # Restart both Harbour processes: session loss, but exact state retained.
    for name in [direct, legacy]:
        docker("exec", name, "./eshop", "//stop")
        docker("wait", name)
        docker("start", name)
    wait_ready(gateway)
    wait_browser(left)
    assert pair(left, right, "/app/main")[0] == 303
    pair(left, right, "/app/login", "POST", dict(user="tester", password="new-secret"))
    assert b"53.34" in pair(left, right, "/app/cart")[2]
    pair(left, right, "/app/shopping?add=0002")
    pair(left, right, "/app/cart?del=0002")
    pair(left, right, "/app/cart")
    logs = docker("logs", gateway)
    for secret in ["test-secret", "new-secret", "wrong-secret", "SESSID", "?add=", "browser.example"]:
        assert secret not in logs, "gateway log disclosure"
    for suffix, upstream, ca in [("ca", "https://legacy:8002", "wrong"), ("hostname", "https://direct:8002", "public")]:
        name = PREFIX + "-bad-" + suffix
        create_gateway(name, upstream, ca)
        browser = Browser(name)
        for _ in range(30):
            try:
                assert browser.request("/hello")[0] == 502
                break
            except (OSError, http.client.HTTPException):
                time.sleep(.1)
        else:
            raise AssertionError("bad trust test unavailable")
        assert subprocess.run(["docker", "exec", name, "eshop-gateway", "probe", "ready"], capture_output=True).returncode != 0
    docker("exec", legacy, "./eshop", "//stop")
    docker("wait", legacy)
    docker("cp", str(TMP / "expired.crt"), legacy + ":/app/certificate.crt")
    docker("start", legacy)
    time.sleep(.5)
    assert right.request("/hello")[0] == 502
    docker("exec", legacy, "./eshop", "//stop")
    docker("wait", legacy)
    docker("cp", str(TMP / "private.crt"), legacy + ":/app/certificate.crt")
    docker("start", legacy)
    wait_ready(gateway)
    docker("exec", legacy, "./eshop", "//stop")
    docker("wait", legacy)
    assert right.request("/hello")[0] == 502
    assert subprocess.run(["docker", "exec", gateway, "eshop-gateway", "probe", "live"], capture_output=True).returncode == 0
    docker("start", legacy)
    wait_ready(gateway)
    # Authentication was lost; verify the current cart before topology rollback.
    assert right.request("/app/login", "POST", dict(user="tester", password="new-secret"))[0] == 303
    assert b"53.34" in right.request("/app/cart")[2]
    # Rehearse rollback using CURRENT volume, public cert, and a single writer.
    docker("stop", "-t", "4", gateway)
    docker("exec", legacy, "./eshop", "//stop")
    docker("wait", legacy)
    docker("rm", legacy)
    rollback = PREFIX + "-rollback"
    create_legacy(rollback, "legacy", PREFIX + "-private", True)
    restored = Browser(rollback)
    for _ in range(30):
        try:
            assert restored.request("/hello")[2] == b"Hello!"
            break
        except OSError:
            time.sleep(.2)
    assert restored.request("/app/main")[0] == 303
    assert restored.request("/app/login", "POST", dict(user="tester", password="new-secret"))[0] == 303
    assert b"Edited" in restored.request("/app/account")[2]
    assert b"53.34" in restored.request("/app/cart")[2]
    print("PASS: protocol/browser parity, TLS trust, probes, restart, isolation, log redaction, rollback")


if __name__ == "__main__":
    main()
