#!/usr/bin/env python3
"""Exercise opt-in launcher, image contracts and current-state restart rollback."""
import argparse
import os
from pathlib import Path
import signal
import ssl
import subprocess
import tempfile
import time
import json
from compatibility import Client, certificate, command, wait, ROOT


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--legacy-image", required=True)
    parser.add_argument("--gateway-image", required=True)
    args = parser.parse_args()
    legacy_id = command("docker", "image", "inspect", "--format", "{{.Id}}", args.legacy_image)
    gateway_id = command("docker", "image", "inspect", "--format", "{{.Id}}", args.gateway_image)
    launcher = ROOT / "scripts/run-gateway.sh"
    assert subprocess.run([str(launcher)], capture_output=True).returncode == 2
    with tempfile.TemporaryDirectory(prefix="eshop-deployment-") as temporary:
        directory = Path(temporary)
        # The gateway's non-root UID must traverse/read only its mounted material.
        directory.chmod(0o755)
        runtime = directory / "runtime"
        credentials = directory / "credentials"
        runtime.mkdir()
        credentials.mkdir()
        seed = f"eshop-seed-{os.getpid()}"
        command("docker", "create", "--name", seed, legacy_id)
        try:
            command("docker", "cp", seed + ":/app/.", str(runtime))
        finally:
            command("docker", "rm", seed)
        certificate(runtime, "certificate", "DNS:legacy,DNS:localhost")
        (runtime / "certificate.key").rename(runtime / "private.key")
        certificate(credentials, "public", "DNS:localhost")
        (credentials / "upstream-ca.crt").write_bytes((runtime / "certificate.crt").read_bytes())
        (credentials / "public.key").chmod(0o644)  # Synthetic fixture only.
        config = directory / "all-legacy.toml"
        config.write_text((ROOT / "gateway/config/all-legacy.toml").read_text())
        # A remote Docker daemon has a different filesystem. Stage only synthetic
        # fixtures there through docker cp; no dependency installation required.
        helper = f"eshop-fixtures-{os.getpid()}"
        command("docker", "run", "-d", "--name", helper, "--mount", "type=bind,src=/tmp,dst=/daemon-tmp",
                "--entrypoint", "sleep", legacy_id, "300")
        command("docker", "cp", "-a", str(directory), helper + ":/daemon-tmp/")
        log = open(directory / "launcher.log", "w+")
        process = subprocess.Popen([str(launcher), "--activate", legacy_id, gateway_id,
                                    str(config), str(credentials), str(runtime)], stdout=log, stderr=log)
        legacy = f"eshop-legacy-{process.pid}"
        gateway = f"eshop-gateway-{process.pid}"
        network = f"eshop-private-{process.pid}"
        try:
            wait(lambda: Client(8002, credentials / "public.crt").request("/hello")[0][2] == b"Hello!", seconds=60)
            legacy_info = json.loads(command("docker", "inspect", legacy))[0]
            gateway_info = json.loads(command("docker", "inspect", gateway))[0]
            assert not legacy_info["HostConfig"]["PortBindings"]
            assert list(gateway_info["HostConfig"]["PortBindings"]) == ["8002/tcp"]
            assert gateway_info["HostConfig"]["ReadonlyRootfs"]
            assert all(not mount["RW"] for mount in gateway_info["Mounts"])
            assert {mount["Destination"] for mount in gateway_info["Mounts"]} == {"/config.toml", "/credentials"}
            topology = json.loads(command("docker", "network", "inspect", network))[0]
            assert topology["Internal"] and len(topology["Containers"]) == 2
            ingress = json.loads(command("docker", "network", "inspect", f"eshop-ingress-{process.pid}"))[0]
            assert len(ingress["Containers"]) == 1
            client = Client(8002, credentials / "public.crt")
            client.request("/app/register", form="user=rollback&name=Retained&password1=secret&password2=secret")
            client.request("/app/shopping?add=0001")
            assert b"26.67" in client.request("/app/cart")[-1][2]
            # Refuse a second writer through the launcher.
            duplicate = subprocess.run([str(launcher), "--activate", legacy_id, gateway_id, str(config), str(credentials), str(runtime)], capture_output=True, timeout=5)
            assert duplicate.returncode != 0
            process.send_signal(signal.SIGTERM)
            process.wait(timeout=15)
            assert not (runtime / ".gateway-writer-lock").exists()
            assert subprocess.run(["docker", "inspect", legacy], capture_output=True).returncode != 0
            assert subprocess.run(["docker", "network", "inspect", network], capture_output=True).returncode != 0
            # Recorded legacy image, current exclusive runtime, direct publication.
            rollback = f"eshop-rollback-{os.getpid()}"
            command("docker", "run", "-d", "--name", rollback, "-p", "127.0.0.1:8002:8002",
                    "--mount", f"type=bind,src={runtime},dst=/app", legacy_id)
            try:
                direct = Client(8002, runtime / "certificate.crt")
                direct.cookies = dict(client.cookies)
                wait(lambda: direct.request("/hello")[0][0] == 200)
                assert direct.request("/app/cart", follow=False)[0][0] == 303
                direct.request("/app/login", form="user=rollback&password=secret")
                assert b"Retained" in direct.request("/app/account")[-1][2]
                assert b"26.67" in direct.request("/app/cart")[-1][2]
            finally:
                command("docker", "rm", "-f", rollback)
            # Failed startup cleans up private writer/network while retaining DBFs.
            bad = directory / "invalid.toml"
            bad.write_text(config.read_text().replace("enabled_families = []", 'enabled_families = ["unknown"]'))
            command("docker", "cp", str(bad), helper + f":/daemon-tmp/{directory.name}/invalid.toml")
            failed = subprocess.run([str(launcher), "--activate", legacy_id, gateway_id,
                                     str(bad), str(credentials), str(runtime)], capture_output=True, timeout=60)
            assert failed.returncode != 0 and not (runtime / ".gateway-writer-lock").exists()
            print("PASS: opt-in launcher, immutable images, private upstream/management, exclusive read-only gateway mounts, writer lock, failure cleanup, restart rollback with current DBFs and session loss")
        except Exception:
            log.flush()
            log.seek(0)
            print(log.read())
            raise
        finally:
            if process.poll() is None:
                process.terminate()
                process.wait(timeout=15)
            command("docker", "exec", helper, "rm", "-rf", f"/daemon-tmp/{directory.name}")
            command("docker", "rm", "-f", helper)
            log.close()


if __name__ == "__main__":
    main()
