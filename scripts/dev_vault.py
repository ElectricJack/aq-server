"""Generate and safely replay the local development vault and its private files."""
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import stat
import tempfile
import urllib.parse

from migration_catalog import state_format

ROOT = Path(__file__).resolve().parents[1]
WORKSPACE = "e0000000-0000-4000-8000-000000000001"


def private_file(path, content):
    """Create once, refusing unsafe retained files without printing their bytes."""
    try:
        metadata = path.lstat()
        if not private_regular_file(metadata):
            raise ValueError("unsafe retained development settlement file") from None
    except FileNotFoundError:
        temporary = None
        try:
            with tempfile.NamedTemporaryFile(dir=path.parent, prefix=".dev-settlement-", delete=False) as out:
                temporary = Path(out.name)
                out.write(content)
                out.flush()
                os.fsync(out.fileno())
            os.link(temporary, path)
        finally:
            if temporary is not None:
                temporary.unlink()


def private_regular_file(metadata):
    identity = (stat.S_ISREG(metadata.st_mode), metadata.st_nlink, metadata.st_uid)
    return identity == (True, 1, os.getuid()) and not (metadata.st_mode & 0o077) and metadata.st_size > 0


def paths():
    state = Path(os.environ.get("AQ_DEV_STATE_DIR", ROOT / "state.dev")).resolve()
    secret = Path(os.environ.get("AQ_SECRETS_DIR", ROOT / "secrets.dev")).resolve()
    vault = Path(os.environ.get("AQ_VAULT_DIR", state / "vault")).resolve()
    return state, secret, vault


def intelligence_section():
    """Opt-in operator [intelligence] tables (MS1: a subscription-login
    credential and its material store). Configuration only, never login bytes:
    those enter through `aq-server import-login`."""
    name = os.environ.get("AQ_DEV_INTELLIGENCE_FILE")
    if not name:
        return "", ""
    text, tables = operator_tables(Path(name))
    # MS1: optionally also the matching [profiled_execution] tables and the
    # private result-media store the dashboard's Results view requires.
    if "intelligence" not in tables or not set(tables) <= {"intelligence", "intelligence_preferences_file", "profiled_execution", "result_media", "provider_quota"}:
        raise ValueError("AQ_DEV_INTELLIGENCE_FILE must contain only [intelligence] (and [profiled_execution], [result_media]) tables")
    prefix, text = separate_root_option(tables, text)
    return prefix, "\n" + text if text.endswith("\n") else "\n" + text + "\n"


def operator_tables(path):
    import tomllib
    try:
        metadata = path.lstat()
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_size > 2 * 1024 * 1024:
            raise ValueError()
        text = path.read_text()
        tables = tomllib.loads(text)
    except (OSError, ValueError, UnicodeDecodeError):
        raise ValueError("AQ_DEV_INTELLIGENCE_FILE must be a small regular TOML file with only [intelligence] tables") from None
    return text, tables


def separate_root_option(tables, text):
    prefix = ""
    if "intelligence_preferences_file" in tables:
        path = tables["intelligence_preferences_file"]
        if not isinstance(path, str) or not Path(path).is_absolute():
            raise ValueError("intelligence_preferences_file must be an absolute metadata path")
        text, count = re.subn(r'(?m)^intelligence_preferences_file\s*=.*(?:\n|$)', '', text)
        if count != 1:
            raise ValueError("Use a single-line intelligence_preferences_file assignment")
        prefix = "intelligence_preferences_file = " + json.dumps(path) + "\n"
    return prefix, text


def public_origin():
    """The CSRF origin is the published admin URL (AQ_DEV_SERVER_URL), never
    vault.example's 127.0.0.1:8080: a stack on another port would refuse every
    browser write (QA 2026-11-06 section 1)."""
    port = os.environ.get("AQ_DEV_ADMIN_PORT", "8080")
    url = urllib.parse.urlsplit(os.environ.get("AQ_DEV_SERVER_URL") or f"http://127.0.0.1:{port}")
    try:
        url.port
    except ValueError:
        url = None
    if not valid_origin(url):
        raise ValueError("AQ_DEV_SERVER_URL must be an http(s) origin such as http://127.0.0.1:28080")
    return f"{url.scheme}://{url.netloc}"


def has_url_parameters(url):
    return any((url.username, url.password, url.query, url.fragment))


def valid_origin(url):
    if url is None or url.scheme not in ("http", "https"):
        return False
    return bool(url.hostname) and not has_url_parameters(url) and url.path in ("", "/")


def origin_line(config):
    lines = config.split("\n")
    matches = [i for i, line in enumerate(lines) if line.startswith("public_origin = ")]
    if len(matches) != 1:
        raise ValueError("development vault needs exactly one [server] public_origin")
    return lines, matches[0]


def with_origin(config, origin):
    lines, index = origin_line(config)
    lines[index] = f'public_origin = "{origin}"'
    return "\n".join(lines)


def prepare():
    state, secret, vault = paths()
    prepare_directories(state, secret, vault)
    prepare_credentials(secret)
    write_vault(vault)
    internal = internal_config(state)
    return dict(**internal, AQ_STATE_FORMAT=str(state_format(ROOT)), AQ_DEV_STATE_DIR=str(state),
                AQ_VAULT_DIR=str(vault), AQ_SECRETS_DIR=str(secret),
                AQ_UPDATER_SPOOL=str(state / "updater-spool"), AQ_DEV_UID=str(os.getuid()),
                AQ_DEV_GID=str(os.getgid()), AQ_DEV_GRAPH_WORKSPACE=WORKSPACE,
                AQ_SERVER_IMAGE=os.environ.get("AQ_SERVER_IMAGE", "agent-q-server:dev"))


def prepare_directories(state, secret, vault):
    for directory in (state, secret, vault,
                      state / "updater-spool/requests", state / "updater-spool/status"):
        directory.mkdir(parents=True, exist_ok=True, mode=0o700)
        directory.chmod(0o700)
    # Only disposable, non-secret test repositories are mounted here. The
    # private state parent still excludes other host users.
    (state / "fixture").mkdir(exist_ok=True, mode=0o755)
    (state / "fixture").chmod(0o755)


def prepare_credentials(secret):
    for name, mode in (("postgres_password", 0o644), ("graph_database_password", 0o644), ("graph_token", 0o600)):
        prepare_token_file(secret / name, mode)
    private_file(secret / "playbook_database_password", secrets.token_hex(32).encode())
    private_file(secret / "playbook_engine_keys", json.dumps(dict(active_key_id="dev-engine-1",
        keys={"dev-engine-1": secrets.token_hex(32)})).encode())
    private_file(secret / "playbook_bridge_key", secrets.token_bytes(32))
    private_file(secret / "terminal_protection_key", secrets.token_bytes(32))


def intact_token(path):
    return not path.is_symlink() and path.is_file() and path.stat().st_size >= 32


def prepare_token_file(path, mode):
    try:
        with path.open("x") as out:
            out.write(secrets.token_hex(32))
    except FileExistsError:
        if not intact_token(path):
            raise ValueError("invalid existing development credential file")
    path.chmod(mode)


def write_vault(vault):
    origin = public_origin()
    target = vault / "config.toml"
    if target.exists() and os.environ.get("AQ_DEV_REPLACE_OPERATOR_SECTION") != "1":
        # A replay keeps the stack's first origin (smoke stacks publish
        # ephemeral ports only after `up`); explicit replacement applies a new one.
        lines, index = origin_line(target.read_text())
        origin = lines[index].removeprefix("public_origin = ").strip('"')
    config = with_origin((ROOT / "vault.example/config.toml").read_text(), origin)
    config += '''
[agent_access]
graph_url = "http://aq-graph:8741"
graph_token_file = "/run/aq-dev/graph_token"
mutation_actor_id = "dev-service"
[source_retention]
staging_root = "/var/lib/aq/source-staging"
git_root = "/var/lib/aq/git"
git_socket = "/run/aq-git/central.sock"
[terminals.protection]
active_key_id = "dev-terminal-1"
[[terminals.protection.keys]]
id = "dev-terminal-1"
file = "/run/aq-dev/terminal_protection_key"
'''
    generated = config
    root_options, operator_tables = intelligence_section()
    config = root_options + config + operator_tables
    if target.exists() and target.read_text() != config:
        # MS1: an operator may replace only the appended operator section of an
        # existing vault (revisioned [intelligence]/[profiled_execution]); every
        # generated table must be byte-identical.
        # The origin follows the published URL, so an older vault with the
        # template origin is updated here too, in both copies.
        previous = target.read_text()
        if not (os.environ.get("AQ_DEV_REPLACE_OPERATOR_SECTION") == "1"
                and re.sub(r'(?m)^intelligence_preferences_file\s*=.*(?:\n|$)', '', with_origin(previous, origin)).startswith(generated)):
            raise ValueError("development vault differs from generated configuration; preserve it and use a new AQ_DEV_STATE_DIR")
        update_settlement_config(vault, previous, config)
    target.write_text(config)


def update_settlement_config(vault, previous, config):
    # The settlement config is config.toml plus [playbooks]: carry it over.
    playbooks = vault / "playbooks.toml"
    if playbooks.exists():
        retained = playbooks.read_text()
        if not retained.startswith(previous):
            raise ValueError("retained settlement configuration differs; use a fresh development stack")
        playbooks.write_text(config + retained[len(previous):])


def internal_config(state):
    """Opt-in internal work host config (docs/operations/internal-work.md).

    Written to <state>/internal/internal.json (0600) only when
    AQ_DEV_INTERNAL_DATABASE_URL and at least one harness are given by
    AQ_DEV_INTERNAL_{CODEX,CLAUDE}_NATIVE and _LOGIN (absolute paths; the native
    executable is hashed here). Returns {} and writes nothing otherwise.

    The file is owned by the invoking developer, mode 0600, and holds the
    database URL (a credential): do not commit or share it. The host refuses a
    config file (and login files) not owned by the uid it runs as, which is
    root in the container, so make a root-owned copy before mounting it
    (`chown root:root`, `chmod 0600`; see docs/operations/internal-work.md).
    """
    url = os.environ.get("AQ_DEV_INTERNAL_DATABASE_URL")
    config = dict(database_url=url, homes="/internal/homes", scratch="/internal/scratch",
                  poll_ms=int(os.environ.get("AQ_DEV_INTERNAL_POLL_MS", "200")))
    # Container paths of compose.workers.yaml; the host-side inputs are mounted there.
    mounts = dict(codex=("/opt/codex/codex", "/logins/codex/"), claude=("/opt/claude/claude", "/logins/claude/"))
    for name, (native_target, login_dir) in mounts.items():
        native = os.environ.get(f"AQ_DEV_INTERNAL_{name.upper()}_NATIVE")
        login = os.environ.get(f"AQ_DEV_INTERNAL_{name.upper()}_LOGIN")
        if native and login:
            config[name] = dict(native=native_target,
                                native_digest=hashlib.sha256(Path(native).read_bytes()).hexdigest(),
                                secret_file=login_dir + Path(login).name)
    if not url or not config.keys() & mounts.keys():
        return {}
    directory = state / "internal"
    directory.mkdir(exist_ok=True, mode=0o700)
    directory.chmod(0o700)
    # Replaced on every prepare: the digests follow the supplied executables.
    target = directory / "internal.json"
    temporary = directory / ".internal.json.new"
    descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(descriptor, "w") as out:
        json.dump(config, out, sort_keys=True)
    os.replace(temporary, target)
    return dict(AQ_INTERNAL_CONFIG_DIR=str(directory))
