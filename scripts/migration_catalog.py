"""Read image-label metadata from the same directory validated by build.rs."""
import argparse
from pathlib import Path
import re


ROOT = Path(__file__).resolve().parents[1]


def state_format(root=ROOT):
    paths = sorted((root / "crates/aq-server/migrations").glob("*.sql"))
    if not paths:
        raise ValueError("migration catalog is empty")
    for expected, path in enumerate(paths, 1):
        match = re.fullmatch(r"([0-9]{4})_([a-z0-9_]+)\.sql", path.name)
        if not match:
            raise ValueError(f"{path.name}: expected NNNN_lowercase_description.sql")
        number = int(match[1])
        if number != expected:
            raise ValueError(f"{path.name}: expected migration {expected:04}, found {number:04}; "
                             "duplicate, missing or out-of-order migration number")
    return len(paths)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    args = parser.parse_args()
    try:
        print(state_format(args.root))
    except ValueError as error:
        parser.exit(1, f"{error}\n")
