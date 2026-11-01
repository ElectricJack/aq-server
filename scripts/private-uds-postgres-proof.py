#!/usr/bin/env python3
"""Isolated cached-PG17 transport proof. No canonical endpoint/config/database inputs."""
import argparse
import json
import os
from pathlib import Path
import secrets
import socket
import stat
import struct
import subprocess
import tempfile
import time
import uuid
from urllib.parse import urlencode

IMAGE = "sha256:99445113a4a19e82dd3713c65bd9f6fb5bf972065ab043df8d1e1c93e957da9a"
PREFIX = "aq-private-uds-"
LABEL = "aq.proof.optional-uds"
PGDATA = "/var/lib/postgresql/data"


def require(value, message):
    if not value:
        raise RuntimeError(message)


def run(args, *, check=True):
    result = subprocess.run(args, text=True, capture_output=True, timeout=45)
    if check and result.returncode:
        # Do not echo command/environment or private SQL/config in errors.
        raise RuntimeError(f"{args[0]} operation failed (exit {result.returncode})")
    return result


def private(path, value):
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW
    with os.fdopen(os.open(path, flags, 0o600), "w") as stream:
        stream.write(value)


def save(root, value):
    # Replace only this owned manifest, never follow an existing link.
    temp = root / f".manifest-{secrets.token_hex(8)}"
    private(temp, json.dumps(value, indent=2) + "\n")
    os.replace(temp, root / "manifest.json")


def load(value):
    root = Path(value)
    require(root.is_absolute() and root == root.resolve() and root.name.startswith(PREFIX), "Unexpected private root")
    metadata = root.lstat()
    require(stat.S_ISDIR(metadata.st_mode) and metadata.st_uid == os.geteuid() and stat.S_IMODE(metadata.st_mode) == 0o700, "Unsafe private root")
    path = root / "manifest.json"
    metadata_file = path.lstat()
    require(stat.S_ISREG(metadata_file.st_mode) and metadata_file.st_nlink == 1 and metadata_file.st_uid == os.geteuid() and stat.S_IMODE(metadata_file.st_mode) == 0o600 and metadata_file.st_size < 8192, "Unsafe proof manifest")
    with os.fdopen(os.open(path, os.O_RDONLY | os.O_NOFOLLOW)) as stream:
        data = json.load(stream)
    require(data["schema_version"] == 1 and data["image"] == IMAGE and data["root"] == str(root), "Foreign proof manifest")
    require(data["uid"] == os.geteuid() and data["gid"] == os.getegid() and data["root_device"] == metadata.st_dev and data["root_inode"] == metadata.st_ino, "Private root identity changed")
    require(uuid.UUID(data["proof_id"]).hex == data["proof_id"].replace("-", ""), "Invalid proof identity")
    require(data["name"] == PREFIX + data["proof_id"] and data["database"] == "aq_uds_" + uuid.UUID(data["proof_id"]).hex, "Foreign resource identity")
    return root, data


def container(root, data):
    result = run(["docker", "inspect", data["name"]], check=False)
    if result.returncode:
        # Absence is confirmed through the Docker inventory, not an arbitrary error.
        listing = run(["docker", "ps", "-aq", "--filter", "name=^/" + data["name"] + "$"])
        require(not listing.stdout.strip(), "Cannot inspect existing proof container")
        return None
    info = json.loads(result.stdout)[0]
    require((data["container_id"] is None or info["Id"] == data["container_id"]) and info["Image"] == IMAGE, "Proof container identity changed")
    require(info["Config"]["Labels"].get(LABEL) == data["proof_id"] and info["Config"]["User"] == f'{data["uid"]}:{data["gid"]}', "Foreign proof container")
    host = info["HostConfig"]
    require(host["NetworkMode"] == "none" and not host["Privileged"] and host["ReadonlyRootfs"] and not host.get("CapAdd") and "ALL" in host["CapDrop"], "Unsafe proof container policy")
    require(not host.get("PortBindings") and "no-new-privileges" in host["SecurityOpt"], "Unsafe proof network/privilege settings")
    expected = {PGDATA: (str(root / "data"), True), "/aq-uds": (str(root / "socket"), True), "/etc/passwd": (str(root / "passwd"), False), "/etc/group": (str(root / "group"), False)}
    seen = set()
    for mount in info["Mounts"]:
        if mount["Type"] == "tmpfs" and mount["Destination"] == "/tmp":
            continue
        require(mount["Type"] == "bind" and expected.get(mount["Destination"]) == (mount["Source"], mount["RW"]), "Proof mount identity changed")
        seen.add(mount["Destination"])
    require(seen == set(expected), "Missing proof bind mount")
    return info


def common(root, data):
    return ["--pull=never", "--network=none", "--user", f'{data["uid"]}:{data["gid"]}', "--cap-drop=ALL", "--security-opt=no-new-privileges", "--read-only", "--pids-limit=128", "--memory=512m", "--cpus=1", "--tmpfs", f'/tmp:rw,nosuid,nodev,noexec,mode=700,uid={data["uid"]},gid={data["gid"]}',
            "--mount", f'type=bind,source={root / "data"},target={PGDATA}',
            "--mount", f'type=bind,source={root / "socket"},target=/aq-uds',
            "--mount", f'type=bind,source={root / "passwd"},target=/etc/passwd,readonly',
            "--mount", f'type=bind,source={root / "group"},target=/etc/group,readonly']


def sql(data, statement, database=None):
    return run(["docker", "exec", "--user", f'{data["uid"]}:{data["gid"]}', data["name"], "psql", "--no-psqlrc", "--no-password", "-XAt", "-v", "ON_ERROR_STOP=1", "-h", "/aq-uds", "-p", "5432", "-U", "postgres", "-d", database or data["database"], "-c", statement]).stdout.strip()


def inspect(root, data):
    info = container(root, data)
    require(info is not None and info["State"]["Running"], "Proof PostgreSQL is not running")
    directory = root / "socket"
    before = directory.lstat()
    require(stat.S_ISDIR(before.st_mode) and before.st_uid == data["uid"] and stat.S_IMODE(before.st_mode) == 0o700, "Unsafe UDS directory")
    held = os.open(directory, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        endpoint = directory / ".s.PGSQL.5432"
        point = endpoint.lstat()
        require(stat.S_ISSOCK(point.st_mode) and point.st_uid == data["uid"] and stat.S_IMODE(point.st_mode) == 0o600, "Unsafe PostgreSQL socket")
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(3)
            client.connect(f"/proc/self/fd/{held}/.s.PGSQL.5432")
            pid, uid, gid = struct.unpack("3i", client.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, struct.calcsize("3i")))
            require(uid == data["uid"] and gid == data["gid"], "Unexpected PostgreSQL peer credentials")
        identity = json.loads(sql(data, "SELECT json_build_object('database',current_database(),'system_identifier',(pg_control_system()).system_identifier::text,'version',current_setting('server_version'),'port',current_setting('port'),'listen_addresses',current_setting('listen_addresses'),'unix_connection',inet_server_addr() IS NULL)::text"))
        require(identity["database"] == data["database"] and identity["port"] == "5432" and identity["listen_addresses"] == "" and identity["unix_connection"], "Unexpected physical PostgreSQL endpoint")
        if "system_identifier" in data:
            require(identity["system_identifier"] == data["system_identifier"], "Physical cluster identity changed")
        data["system_identifier"] = identity["system_identifier"]
        methods = sql(data, "SELECT string_agg(DISTINCT auth_method,',' ORDER BY auth_method) FROM pg_hba_file_rules WHERE type='local'")
        require(methods == "peer", "Unexpected local authentication policy")
        after = directory.lstat()
        require((after.st_dev, after.st_ino) == (before.st_dev, before.st_ino) == (os.fstat(held).st_dev, os.fstat(held).st_ino), "UDS directory changed")
        result = {"passed": True, "scope": "isolated PostgreSQL transport only; no AQ/runtime/namespace startup yet", "image": IMAGE, "container_id": info["Id"], "peer_pid": pid, "peer_uid": uid, "peer_gid": gid, "socket_uid": point.st_uid, "socket_mode": "0600", "directory_mode": "0700", "postgres": identity, "local_auth": methods, "tcp_listener": False, "published_ports": False}
        save(root, data)
        temp = root / f".transport-{secrets.token_hex(8)}"
        private(temp, json.dumps(result, indent=2) + "\n")
        os.replace(temp, root / "transport-result.json")
        return result
    finally:
        os.close(held)


def create():
    require(os.geteuid() == 1000 and os.getegid() == 1000, "This reviewed local proof requires explicit UID/GID1000")
    image = json.loads(run(["docker", "image", "inspect", IMAGE]).stdout)[0]
    require(image["Id"] == IMAGE, "Cached PG17 image changed")
    root = Path(tempfile.mkdtemp(prefix=PREFIX))
    for name in ["data", "socket"]:
        (root / name).mkdir(mode=0o700)
    private(root / "passwd", f"root:x:0:0:root:/root:/bin/false\npostgres:x:1000:1000:PostgreSQL:{PGDATA}:/bin/false\n")
    private(root / "group", "root:x:0:\npostgres:x:1000:\n")
    proof = str(uuid.uuid4()); metadata = root.stat()
    data = {"schema_version": 1, "proof_id": proof, "name": PREFIX + proof, "image": IMAGE, "root": str(root), "uid": 1000, "gid": 1000, "root_device": metadata.st_dev, "root_inode": metadata.st_ino, "database": "aq_uds_" + uuid.UUID(proof).hex, "state": "prepared", "container_id": None}
    save(root, data)
    # One labelled lifecycle includes initdb; a failed/uncertain CLI cannot strand
    # an unnamed initialization container. No image entrypoint permission changes.
    script = """set -eu
if [ ! -f /var/lib/postgresql/data/PG_VERSION ]; then
/usr/lib/postgresql/17/bin/initdb -D /var/lib/postgresql/data -U postgres --auth-local=peer --auth-host=reject --no-locale --encoding=UTF8
fi
exec /usr/lib/postgresql/17/bin/postgres -D /var/lib/postgresql/data -c listen_addresses= -c unix_socket_directories=/aq-uds -c unix_socket_permissions=0600 -c max_connections=64 -c shared_buffers=32MB
"""
    result = run(["docker", "run", "-d", "--name", data["name"], "--label", f"{LABEL}={proof}", *common(root, data), "--entrypoint", "/bin/sh", IMAGE, "-c", script])
    data["container_id"] = result.stdout.strip(); data["state"] = "running"; save(root, data)
    try:
        # Every probe is finite; no TCP or published-port fallback.
        deadline = time.monotonic() + 15
        while True:
            try:
                sql(data, "SELECT 1", database="postgres"); break
            except RuntimeError:
                require(time.monotonic() < deadline, "Private PostgreSQL did not become ready")
                time.sleep(0.1)
        sql(data, 'CREATE DATABASE "' + data["database"] + '"', database="postgres")
        params = urlencode({"host": str(root / "socket"), "port": "5432", "sslmode": "disable", "application_name": "aq-private-uds-proof"})
        private(root / "aq-database-url", f'postgres://postgres@localhost/{data["database"]}?{params}\n')
        private(root / "engine-connection", f'host=\'{root / "socket"}\' port=5432 user=postgres dbname={data["database"]} sslmode=disable\n')
        inspect(root, data)
    except Exception:
        print(json.dumps({"private_root": str(root), "cleanup": "stop this exact labelled proof container; private files retained"}))
        raise
    print(json.dumps({"private_root": str(root), "state": "transport_proven", "result": str(root / "transport-result.json")}))


def stop(root, data):
    info = container(root, data)
    if info is not None:
        run(["docker", "stop", "--timeout=10", info["Id"]])
        run(["docker", "rm", info["Id"]])
    data["state"] = "stopped"; save(root, data)
    print(json.dumps({"private_root": str(root), "state": "stopped", "private_data_retained": True}))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["create", "inspect", "stop"])
    parser.add_argument("--root", help="Existing private root printed by create (inspect/stop only)")
    args = parser.parse_args()
    if args.command == "create":
        require(args.root is None, "create always generates a new private root")
        create()
    else:
        require(args.root is not None, "Existing proof root required")
        root, data = load(args.root)
        if args.command == "inspect": print(json.dumps(inspect(root, data)))
        else: stop(root, data)


if __name__ == "__main__":
    main()
