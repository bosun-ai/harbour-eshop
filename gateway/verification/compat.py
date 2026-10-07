#!/usr/bin/env python3
"""Disposable stdlib assertions; no redirects, response rewriting, or secret reports."""
import http.client
import json
import os
from pathlib import Path
import re
import socket
import ssl
import subprocess
import sys
import time
import threading
import urllib.parse

CA, PREFIX, IMAGE = sys.argv[1:]
TLS = ssl.create_default_context(cafile=CA)


def docker(*args, check=True):
    return subprocess.run(["docker", *args], check=check, capture_output=True, text=True)


class Browser:
    def __init__(self, port):
        self.port = port
        self.cookie = ""

    def request(self, path, method="GET", data=None, headers=None, chunked=False):
        fields = {"Host": "localhost:8002"}
        fields.update(headers or {})
        if self.cookie:
            fields["Cookie"] = self.cookie
        body = None
        if data is not None:
            body = urllib.parse.urlencode(data).encode()
            fields["Content-Type"] = "application/x-www-form-urlencoded"
            if chunked:
                body = iter([body])
        connection = http.client.HTTPSConnection("localhost", self.port, context=TLS, timeout=15)
        connection.request(method, path, body, fields, encode_chunked=chunked)
        response = connection.getresponse()
        result = response.status, response.getheaders(), response.read()
        connection.close()
        for name, value in result[1]:
            if name.lower() == "set-cookie":
                self.cookie = value.split(";", 1)[0]
        return result


def header(result, name):
    return [value for field, value in result[1] if field.lower() == name.lower()]


def stable(result):
    cookies = [value.split(";", 1)[1:] for value in header(result, "set-cookie")]
    return result[0], header(result, "location"), header(result, "content-type"), cookies, result[2]


def wait(browser):
    for _ in range(60):
        try:
            if browser.request("/hello")[2] == b"Hello!":
                return
        except (OSError, http.client.HTTPException):
            pass
        time.sleep(0.5)
    raise AssertionError("listener not ready")


direct, proxy = Browser(18003), Browser(18002)
wait(direct)
wait(proxy)
for path, method in [
    ("/hello", "GET"), ("/", "GET"), ("/app/login", "GET"),
    ("/app/register", "GET"), ("/unknown", "GET"),
    ("/hello", "HEAD"), ("/hello", "OPTIONS"),
    ("/files/main.css", "GET"), ("/files/missing.css", "GET"),
    ("/app/login?err&raw=%2f%2B+%25&raw=second", "GET"),
]:
    assert stable(direct.request(path, method)) == stable(proxy.request(path, method)), (path, method)
css = direct.request("/files/main.css")
modified = header(css, "last-modified")
assert modified, "static last-modified missing"
conditional = {"If-Modified-Since": modified[0]}
assert stable(direct.request("/files/main.css", headers=conditional)) == stable(proxy.request("/files/main.css", headers=conditional))
assert direct.request("/hello", "HEAD")[0] == 501

# Separate exclusively owned databases receive the same scenario once each.
def scenario(browser):
    results = []

    def call(path, method="GET", data=None, location=None):
        result = browser.request(path, method, data)
        if location is not None:
            assert result[0] == 303 and header(result, "location") == [location], (path, result[0])
            # Resolve redirects exactly as a browser would, only after asserting them.
            target = urllib.parse.urljoin(path, location)
            assert browser.request(target)[0] in (200, 303)
        results.append(stable(result))
        return result

    user = {"user": "bootstrap", "name": "Synthetic", "password1": "testpass", "password2": "testpass"}
    call("/app/main", location="/app/login")
    call("/app/register", "POST", {}, "?err=1")
    call("/app/register", "POST", dict(user, password2="mismatch"), "?err=2")
    call("/app/register", "POST", user, "/app/main")
    call("/app/register", "POST", user, "?err=3")
    call("/app/account/edit", "POST", {"name": ""}, "?err=1")
    call("/app/account/edit", "POST", {"name": "Retained", "password1": "a", "password2": "b"}, "?err=2")
    call("/app/account/edit", "POST", {"name": "Retained"}, "/app/account")
    # Login applies Harbour's fixed-width identity before composite cart lookups.
    call("/app/login", "POST", {"user": "bootstrap", "password": "testpass"}, "main")
    shopping = call("/app/shopping")
    codes = re.findall(rb'\?add=([^"&<>]+)', shopping[2])
    assert codes, "no seeded items"
    code = codes[0].decode()
    call("/app/shopping?_pos=10")
    call("/app/shopping?add=" + code, location="shopping")
    call("/app/shopping?add=" + code, location="shopping")
    cart = call("/app/cart")
    assert b"53.34" in cart[2], "two adds must total 53.34"
    call("/app/cart?del=" + code, location="cart")
    assert b"53.34" not in call("/app/cart")[2]
    # Retain one cart item for the rollback rehearsal.
    call("/app/shopping?add=" + code, location="shopping")
    call("/app/logout")
    call("/app/login", "POST", {"user": "bootstrap", "password": "wrong"}, "login?err")
    call("/app/login", "POST", {"user": "bootstrap", "password": "testpass"}, "main")
    assert b"Retained" in call("/app/account")[2]
    return results


assert scenario(direct) == scenario(proxy), "stateful differential mismatch"
for browser in (direct, proxy):
    result = browser.request("/app/register", "POST", {"user": "chunked", "name": "Synthetic", "password1": "testpass", "password2": "testpass"}, chunked=True)
    assert result[0] == 303 and header(result, "location") == ["?err=1"], "chunked form changed application effect"

legacy_info = json.loads(docker("inspect", PREFIX + "-legacy").stdout)[0]
gateway_info = json.loads(docker("inspect", PREFIX + "-gateway").stdout)[0]
assert not legacy_info["HostConfig"]["PortBindings"], "private legacy published"
assert all(mount["Destination"] != "/app" for mount in gateway_info["Mounts"])

# Same Harbour process, direct access only from a temporary container on its network.
def private(path, cookie):
    script = 'import http.client,ssl,sys; c=http.client.HTTPSConnection("legacy",8002,context=ssl.create_default_context(cafile="/certs/ca.crt")); c.request("GET",sys.argv[1],headers={"Cookie":sys.argv[2],"Host":"localhost:8002"}); r=c.getresponse(); print(r.status); print(repr(r.read()))'
    return docker("run", "--rm", "--network", PREFIX + "-net", "-v", PREFIX + "-certs:/certs:ro", "--entrypoint", "python3", "python:3.12-slim", "-c", script, path, cookie).stdout


assert private("/app/account", proxy.cookie).startswith("200\n"), "gateway cookie not accepted directly"
# Obtain a direct session using the disposable account (no database mutation).
script = 'import http.client,ssl; c=http.client.HTTPSConnection("legacy",8002,context=ssl.create_default_context(cafile="/certs/ca.crt")); c.request("POST","/app/login","user=bootstrap&password=testpass",{"Content-Type":"application/x-www-form-urlencoded"}); r=c.getresponse(); print(r.getheader("Set-Cookie").split(";")[0])'
cookie = docker("run", "--rm", "--network", PREFIX + "-net", "-v", PREFIX + "-certs:/certs:ro", "--entrypoint", "python3", "python:3.12-slim", "-c", script).stdout.strip()
demo = Browser(18002)
demo.cookie = cookie
assert demo.request("/app/account")[0] == 200
docker("restart", PREFIX + "-gateway")
wait(proxy)
assert proxy.request("/app/account")[0] == 200, "gateway restart lost session"

# Host and diagnostic headers stay opaque, with no injected correlation header.
info = proxy.request("/info?raw=%2f", headers={"X-Probe": "opaque-marker", "Host": "original-host"})
assert b"opaque-marker" in info[2] and b"original-host" in info[2]
assert b"HTTP_X_REQUEST_ID" not in info[2]

# Invalid inputs fail before opening a listener; registration never activates routes.
def failure_gateway(overrides, detached=False):
    args = ["run", "--no-healthcheck", "--name", PREFIX + "-failure", "--network", PREFIX + "-net",
            "-v", PREFIX + "-certs:/certs:ro", "--env-file", str(Path(__file__).resolve().parents[1] / "config/example.env")]
    if detached:
        args += ["-d", "-p", "127.0.0.1:18005:8002"]
    for name, value in overrides.items():
        args += ["-e", name + "=" + value]
    return docker(*args, IMAGE, check=False)


for overrides in [{"GW_ACTIVE_FAMILIES": "cart"}, {"GW_LEGACY_URL": "http://legacy:8002"},
                  {"GW_LEGACY_URL": "https://legacy:99999"},
                  {"GW_LEGACY_URL": "https://legacy:"},
                  {"GW_IDLE_SECONDS": "0"}, {"GW_OPS_BIND": "0.0.0.0:9000"},
                  {"GW_TLS_KEY": "/certs/ca.crt"}]:
    assert failure_gateway(overrides).returncode != 0, "invalid config accepted"
    docker("rm", "-f", PREFIX + "-failure")
for overrides in [{"GW_LEGACY_CA": "/certs/public.crt"},
                  {"GW_LEGACY_URL": "https://" + PREFIX + "-legacy:8002"}]:
    assert failure_gateway(overrides, True).returncode == 0
    time.sleep(1)
    assert docker("exec", PREFIX + "-failure", "eshop-gateway", "check-ready", check=False).returncode != 0
    assert Browser(18005).request("/hello")[0] == 502, "TLS verification bypassed"
    docker("rm", "-f", PREFIX + "-failure")

# Fault upstream counts accepted application requests. A dropped mutation and a
# stalled response must each get exactly one attempt, regardless of GET semantics.
fault_script = '''import socket,ssl,time
ctx=ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ctx.load_cert_chain('/app/certificate.crt','/app/private.key')
s=socket.socket(); s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1); s.bind(('0.0.0.0',8002)); s.listen()
while True:
 try:
  c=ctx.wrap_socket(s.accept()[0],server_side=True); data=b''
  while b'\\r\\n\\r\\n' not in data: data+=c.recv(4096)
  print('ATTEMPT',flush=True)
  if b'drain' in data:
   time.sleep(1); c.sendall(b'HTTP/1.1 200 OK\\r\\nContent-Length: 6\\r\\n\\r\\nHello!')
  elif b'body-idle' in data:
   c.sendall(b'HTTP/1.1 200 OK\\r\\nContent-Length: 6\\r\\n\\r\\nH'); time.sleep(4)
  elif b'stall' in data: time.sleep(4)
  c.close()
 except (OSError,ssl.SSLError): pass
'''
docker("run", "-d", "--name", PREFIX + "-probe", "--network", PREFIX + "-net", "--network-alias", "fault", "-v", PREFIX + "-runtime:/app:ro", "--entrypoint", "python3", "python:3.12-slim", "-c", fault_script)
failure_gateway({"GW_LEGACY_URL": "https://fault:8002", "GW_IDLE_SECONDS": "2", "GW_CONNECT_SECONDS": "2", "GW_TOTAL_SECONDS": "3", "GW_DRAIN_SECONDS": "2"}, True)
time.sleep(1)
fault = Browser(18005)
assert fault.request("/app/shopping?add=0001")[0] == 502
assert fault.request("/app/shopping?add=0001&stall")[0] == 502
time.sleep(3)
assert docker("logs", PREFIX + "-probe").stdout.count("ATTEMPT") == 2, "request retried"
try:
    fault.request("/body-idle")
    raise AssertionError("body idle timeout did not terminate truncated exchange")
except http.client.IncompleteRead:
    pass
time.sleep(3)
assert docker("logs", PREFIX + "-probe").stdout.count("ATTEMPT") == 3
drained = []
thread = threading.Thread(target=lambda: drained.append(fault.request("/drain")))
thread.start()
for _ in range(50):
    if docker("logs", PREFIX + "-probe").stdout.count("ATTEMPT") == 4:
        break
    time.sleep(0.1)
docker("stop", "-t", "5", PREFIX + "-failure")
thread.join(timeout=5)
assert drained and drained[0][2] == b"Hello!", "SIGTERM failed to drain accepted response"
assert json.loads(docker("inspect", PREFIX + "-failure").stdout)[0]["State"]["ExitCode"] == 0
docker("rm", "-f", PREFIX + "-failure")
docker("rm", "-f", PREFIX + "-probe")

# Threaded transport peer: no application behavior or Harbour changes.
deadline_script = r'''import os,socket,ssl,time,threading
ctx=ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ctx.load_cert_chain('/app/certificate.crt','/app/private.key')
def handle(raw):
 c=None
 try:
  time.sleep(float(os.environ.get('TLS_DELAY','0')))
  c=ctx.wrap_socket(raw,server_side=True); c.settimeout(12); data=b''
  while b'\r\n\r\n' not in data: data+=c.recv(4096)
  headers,body=data.split(b'\r\n\r\n',1); path=headers.split()[1]
  if path==b'/large':
   c.sendall(b'HTTP/1.1 200 OK\r\nContent-Length: 67108864\r\n\r\n')
   for _ in range(1024): c.sendall(b'x'*65536)
  elif path==b'/total':
   c.sendall(b'HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n')
   for _ in range(100): c.sendall(b'x'); time.sleep(.4)
  elif path==b'/early':
   c.sendall(b'HTTP/1.1 413 Content Too Large\r\nContent-Length: 0\r\n\r\n')
  else:
   length=next((int(line.split(b':',1)[1]) for line in headers.split(b'\r\n') if line.lower().startswith(b'content-length:')),0)
   while len(body)<length:
    chunk=c.recv(4096)
    if not chunk: break
    body+=chunk
   if path==b'/upload': print('UPLOAD',len(body),flush=True)
   if path==b'/headers': time.sleep(4)
   result=b'Hello!' if path==b'/hello' else body
   c.sendall(b'HTTP/1.1 200 OK\r\nContent-Length: '+str(len(result)).encode()+b'\r\n\r\n'+result)
 except (OSError,ssl.SSLError): pass
 finally:
  if c: c.close()
  else: raw.close()
  print('CLOSED',flush=True)
s=socket.socket(); s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
s.bind(('0.0.0.0',443)); s.listen(512)
while True: threading.Thread(target=handle,args=(s.accept()[0],),daemon=True).start()
'''
docker("run", "-d", "--name", PREFIX + "-probe", "--network", PREFIX + "-net", "--network-alias", "fault", "-v", PREFIX + "-runtime:/app:ro", "--entrypoint", "python3", "python:3.12-slim", "-c", deadline_script)
# Omitted upstream port must actually use 443, not merely parse successfully.
failure_gateway({"GW_LEGACY_URL": "https://fault", "GW_IDLE_SECONDS": "2", "GW_CONNECT_SECONDS": "2", "GW_TOTAL_SECONDS": "8"}, True)
time.sleep(1)
assert docker("exec", PREFIX + "-failure", "eshop-gateway", "check-ready").returncode == 0

def upload_headers(stream, path):
    stream.sendall(b"POST " + path + b" HTTP/1.1\r\nHost: localhost\r\nContent-Length: 6\r\nConnection: close\r\n\r\n")

with TLS.wrap_socket(socket.create_connection(("localhost", 18005)), server_hostname="localhost") as stream:
    upload_headers(stream, b"/upload")
    for byte in b"abcdef":
        time.sleep(.55)
        stream.sendall(bytes([byte]))
    response = http.client.HTTPResponse(stream)
    response.begin()
    assert response.status == 200 and response.read() == b"abcdef", "active upload was cut short"
assert "UPLOAD 6" in docker("logs", PREFIX + "-probe").stdout
with TLS.wrap_socket(socket.create_connection(("localhost", 18005)), server_hostname="localhost") as stream:
    upload_headers(stream, b"/upload")
    stream.sendall(b"a")
    start = time.monotonic()
    response = http.client.HTTPResponse(stream)
    response.begin()
    assert response.status == 502 and time.monotonic() - start < 4, "idle upload not bounded"
    response.read()
with TLS.wrap_socket(socket.create_connection(("localhost", 18005)), server_hostname="localhost") as stream:
    upload_headers(stream, b"/early")
    response = http.client.HTTPResponse(stream)
    response.begin()
    assert response.status == 413, "early response waited for upload"
start = time.monotonic()
assert Browser(18005).request("/headers")[0] == 502
assert time.monotonic() - start < 4, "response-header idle timeout missing"

# Body progress must not extend the independent total connection deadline.
start = time.monotonic()
try:
    Browser(18005).request("/total")
    raise AssertionError("total deadline did not truncate active response")
except http.client.IncompleteRead:
    assert 6 < time.monotonic() - start < 10
time.sleep(1)

# Never read the large response. All 256 public permits must recover, including
# the shared operations capacity; inspect sockets without draining client output.
from concurrent.futures import ThreadPoolExecutor

def stalled_reader(_):
    raw = socket.socket()
    raw.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
    raw.settimeout(10)
    raw.connect(("localhost", 18005))
    stream = TLS.wrap_socket(raw, server_hostname="localhost")
    stream.sendall(b"GET /large HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
    return stream

closed_before = docker("logs", PREFIX + "-probe").stdout.count("CLOSED")
single = stalled_reader(0)
try:
    time.sleep(5)
    assert docker("logs", PREFIX + "-probe").stdout.count("CLOSED") - closed_before == 1, "single stalled reader exceeded write-idle bound"
finally:
    single.close()
closed_before = docker("logs", PREFIX + "-probe").stdout.count("CLOSED")
streams = []
try:
    start = time.monotonic()
    with ThreadPoolExecutor(max_workers=256) as pool:
        streams = list(pool.map(stalled_reader, range(256)))
    time.sleep(11)
    # Linux state 01 is ESTABLISHED; port 8002 is 1F42. FIN_WAIT sockets may
    # retain queued bytes but must no longer own a gateway task/permit.
    sockets = docker("exec", PREFIX + "-failure", "cat", "/proc/net/tcp").stdout
    established = [line for line in sockets.splitlines()[1:] if line.split()[1].endswith(":1F42") and line.split()[3] == "01"]
    assert not established, ("stalled public connections still established", len(established), established[:3], time.monotonic() - start, docker("logs", PREFIX + "-probe").stdout.count("CLOSED") - closed_before)
    assert docker("logs", PREFIX + "-probe").stdout.count("CLOSED") - closed_before == 256, "upstream drivers were not all cancelled"
    assert docker("exec", PREFIX + "-failure", "eshop-gateway", "check-ready").returncode == 0, "permits/readiness did not recover"
    live = docker("run", "--rm", "--network", "container:" + PREFIX + "-failure", "--entrypoint", "python3", "python:3.12-slim", "-c", 'import urllib.request; print(urllib.request.urlopen("http://127.0.0.1:9000/live").status)').stdout
    assert live.strip() == "200", "health capacity did not recover"
    assert Browser(18005).request("/hello")[2] == b"Hello!", "public capacity did not recover"
finally:
    for stream in streams:
        stream.close()
docker("rm", "-f", PREFIX + "-failure")
docker("rm", "-f", PREFIX + "-probe")

# Setup may exceed body-idle while remaining within connect and total limits.
docker("run", "-d", "--name", PREFIX + "-probe", "--network", PREFIX + "-net", "--network-alias", "fault", "-v", PREFIX + "-runtime:/app:ro", "-e", "TLS_DELAY=1.6", "--entrypoint", "python3", "python:3.12-slim", "-c", deadline_script)
failure_gateway({"GW_LEGACY_URL": "https://fault", "GW_IDLE_SECONDS": "1", "GW_CONNECT_SECONDS": "5", "GW_TOTAL_SECONDS": "8"}, True)
time.sleep(1)
assert Browser(18005).request("/hello")[2] == b"Hello!", "delayed TLS setup failed"
for streaming in (False, True):
    with TLS.wrap_socket(socket.create_connection(("localhost", 18005)), server_hostname="localhost") as stream:
        upload_headers(stream, b"/upload")
        if streaming:
            for byte in b"abcdef":
                time.sleep(.55)
                stream.sendall(bytes([byte]))
        else:
            stream.sendall(b"abcdef")
        response = http.client.HTTPResponse(stream)
        response.begin()
        assert response.status == 200 and response.read() == b"abcdef", ("TLS setup consumed upload idle budget", streaming)
assert docker("logs", PREFIX + "-probe").stdout.count("UPLOAD 6") == 2, "delayed TLS uploads were incomplete"
with TLS.wrap_socket(socket.create_connection(("localhost", 18005)), server_hostname="localhost") as stream:
    upload_headers(stream, b"/upload")
    stream.sendall(b"a")
    start = time.monotonic()
    response = http.client.HTTPResponse(stream)
    response.begin()
    assert response.status == 502 and 2.2 < time.monotonic() - start < 4.5, "post-setup idle upload not bounded"
    response.read()
docker("rm", "-f", PREFIX + "-failure")
docker("rm", "-f", PREFIX + "-probe")

# Ambiguous framing must never reach a Harbour application handler.
with TLS.wrap_socket(socket.create_connection(("localhost", 18002)), server_hostname="localhost") as stream:
    stream.sendall(b"POST /app/register HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n0\r\n\r\n")
    assert b" 400 " in stream.recv(1024), "ambiguous framing not rejected"

docker("stop", "-t", "5", PREFIX + "-legacy")
assert docker("exec", PREFIX + "-gateway", "eshop-gateway", "check-ready", check=False).returncode != 0
live = docker("run", "--rm", "--network", "container:" + PREFIX + "-gateway", "--entrypoint", "python3", "python:3.12-slim", "-c", 'import urllib.request; print(urllib.request.urlopen("http://127.0.0.1:9000/live").status)').stdout
assert live.strip() == "200", "upstream outage killed liveness"
outage = proxy.request("/app/shopping?add=unknown")
assert outage[0] == 502 and b"outcome may be unknown" in outage[2]
docker("start", PREFIX + "-legacy")
wait(proxy)
assert proxy.request("/app/account")[0] == 303, "Harbour restart did not invalidate session"
proxy.request("/app/login", "POST", {"user": "bootstrap", "password": "testpass"})
assert b"Retained" in proxy.request("/app/account")[2]
assert b"26.67" in proxy.request("/app/cart")[2]

# Hosting rollback: stop the sole writer, reuse its current full runtime volume.
start = time.monotonic()
docker("stop", "-t", "35", PREFIX + "-gateway")
assert time.monotonic() - start < 35
assert json.loads(docker("inspect", PREFIX + "-gateway").stdout)[0]["State"]["ExitCode"] == 0
docker("stop", "-t", "5", PREFIX + "-legacy")
docker("run", "-d", "--name", PREFIX + "-rollback", "-p", "127.0.0.1:18004:8002", "-v", PREFIX + "-runtime:/app", os.environ.get("LEGACY_IMAGE", "harbour-eshop:legacy"))
rollback = Browser(18004)
wait(rollback)
rollback.request("/app/login", "POST", {"user": "bootstrap", "password": "testpass"})
assert b"Retained" in rollback.request("/app/account")[2]
assert b"26.67" in rollback.request("/app/cart")[2]
print("PASS: HTTP/stateful compatibility, chunking, sessions, isolation, TLS/config/port failures, upload/header/total/write-idle deadlines, 256 stalled-reader permits and health recovery, upstream cancellation, one-attempt faults, outage, lifecycle, retained-data rollback")
