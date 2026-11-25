#!/usr/bin/env python3
"""Install the AQ v2 skill collection from this checkout."""
import argparse
import fcntl
import os
from pathlib import Path
import tempfile


def write_skill(destination: Path, content: bytes) -> None:
    with tempfile.NamedTemporaryFile(dir=destination.parent, delete=False) as output:
        temporary = Path(output.name)
        try:
            output.write(content)
            output.close()
            temporary.chmod(0o644)
            temporary.replace(destination)
        finally:
            temporary.unlink(missing_ok=True)


def validate_target(directory: Path) -> None:
    target = directory / "SKILL.md"
    if directory.is_symlink() or target.is_symlink():
        raise ValueError(f"Refusing to replace a symlink at {directory} or {target}")
    for path, check, expected in (
        (directory, directory.is_dir, "a skill directory"),
        (target, target.is_file, "a regular skill file"),
    ):
        if path.exists() and not check():
            raise ValueError(f"Expected {expected} at {path}")


def install_one(directory: Path, content: bytes) -> None:
    target = directory / "SKILL.md"
    if target.exists():
        current = target.read_bytes()
        if current == content:
            print(f"Current: {directory.name}")
            return
        with tempfile.NamedTemporaryFile(
            dir=directory, prefix="SKILL.md.aq-backup-", delete=False
        ) as backup:
            backup.write(current)
        print(f"Backed up: {backup.name}")
    directory.mkdir(exist_ok=True)
    write_skill(target, content)
    print(f"Installed: {target}")


def install(source: Path, destination: Path) -> None:
    contents = {
        skill.parent.name: skill.read_bytes()
        for skill in sorted(source.glob("aq-*/SKILL.md"))
    }
    if not contents:
        raise ValueError(f"No AQ skill sources found under {source}")

    destination.mkdir(parents=True, exist_ok=True)
    lock = os.open(destination, os.O_RDONLY | os.O_DIRECTORY)
    try:
        fcntl.flock(lock, fcntl.LOCK_EX)
        for name in contents:
            validate_target(destination / name)
        for name, content in contents.items():
            install_one(destination / name, content)
    finally:
        os.close(lock)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    codex_home = Path(os.environ.get("CODEX_HOME") or Path.home() / ".codex")
    parser.add_argument(
        "--dest", type=Path, default=codex_home / "skills",
        help="destination skills directory (default: ${CODEX_HOME:-~/.codex}/skills)",
    )
    args = parser.parse_args()
    try:
        install(Path(__file__).resolve().parents[1] / "skills", args.dest.expanduser())
    except (OSError, ValueError) as error:
        parser.exit(1, f"{parser.prog}: {error}\n")


if __name__ == "__main__":
    main()
