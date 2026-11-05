#!/usr/bin/env python3
"""Verify retained aq-git release inputs; never consult a sibling checkout."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tarfile
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parents[1]


def digest(data):
    return hashlib.sha256(data).hexdigest()


def retained_path(name):
    vendor = ROOT / 'vendor'
    path = vendor / name
    if Path(name).is_absolute() or '..' in Path(name).parts or not path.resolve().is_relative_to(vendor.resolve()):
        raise ValueError('unsafe retained input path')
    return path


def verify_build_stages(manifest):
    """Require application Dockerfiles to embed the exact canonical builders."""
    canonical = (ROOT / 'scripts/Dockerfile.aq-git').read_text()
    stages = list(re.finditer(r'^FROM ', canonical, re.MULTILINE))
    if len(stages) != 3:
        raise ValueError('canonical aq-git recipe must have two builders and one runtime')
    expected = canonical[:stages[2].start()]
    if expected.count('FROM ' + manifest['builder']) != 2:
        raise ValueError('canonical builder disagrees with candidate manifest')
    for key in ('archive_sha256', 'binary_sha256'):
        if manifest[key] not in expected:
            raise ValueError('canonical release checksums disagree with candidate manifest')
    if manifest['git']['sha256'] not in expected:
        raise ValueError('canonical Git checksum disagrees with candidate manifest')
    begin = '# BEGIN PINNED AQ-GIT BUILD STAGES\n'
    end = '# END PINNED AQ-GIT BUILD STAGES\n'
    for name in ('Dockerfile', 'Dockerfile.worker'):
        content = (ROOT / name).read_text()
        if content.count(begin) != 1 or content.count(end) != 1:
            raise ValueError(f'{name}: exactly one pinned aq-git block required')
        actual = content.split(begin, 1)[1].split(end, 1)[0]
        if actual != expected:
            raise ValueError(f'{name}: pinned aq-git builders drifted from scripts/Dockerfile.aq-git')


def verify_bundle(manifest, inventory):
    bundle = retained_path('aq-git-source.bundle')
    if digest(bundle.read_bytes()) != manifest['bundle_sha256']:
        raise ValueError('Git bundle digest mismatch')
    heads = subprocess.check_output(['git', 'bundle', 'list-heads', str(bundle)], text=True).splitlines()
    if f"{manifest['source_revision']} HEAD" not in heads:
        raise ValueError('bundle commit identity mismatch')
    # Read committed blobs without extracting source paths onto the filesystem.
    with tempfile.TemporaryDirectory(prefix='aq-git-verify-') as temp:
        repo = Path(temp) / 'source.git'
        subprocess.run(['git', 'clone', '--bare', '--quiet', str(bundle), str(repo)], check=True)
        with tempfile.TemporaryFile() as archive:
            subprocess.run(['git', '-C', str(repo), 'archive', manifest['source_revision']], stdout=archive, check=True)
            archive.seek(0)
            with tarfile.open(fileobj=archive) as tar:
                files = {m.name: digest(tar.extractfile(m).read()) for m in tar if m.isfile()}
                if any(not (m.isfile() or m.isdir()) for m in tar.getmembers()) or files != inventory:
                    raise ValueError('bundle committed source disagrees with archive inventory')


def candidate(official=False):
    if official and any(os.environ.get(k) for k in ('AQ_GIT_BIN', 'AQ_GIT_SOURCE_DIR')):
        raise ValueError('official builds reject local aq-git overrides')
    m = json.loads((ROOT / 'vendor/git-candidate.json').read_text())
    if not re.fullmatch('[0-9a-f]{40}', m['source_revision']):
        raise ValueError('full revision required')
    binary = retained_path(m['binary'])
    if digest(binary.read_bytes()) != m['binary_sha256']:
        raise ValueError('aq-git binary digest mismatch')
    archive = retained_path(m['archive'])
    if digest(archive.read_bytes()) != m['archive_sha256']:
        raise ValueError('aq-git source digest mismatch')
    prefix = m['archive_prefix']
    if prefix != f"aq-git-v{m['version']}/":
        raise ValueError('source archive prefix disagrees with version')
    with tarfile.open(archive) as tar:
        members = tar.getmembers()
        for member in members:
            if (Path(member.name).is_absolute() or '..' in Path(member.name).parts
                    or not (member.isfile() or member.isdir())
                    or not (member.name == prefix.rstrip('/') or member.name.startswith(prefix))):
                raise ValueError('unsafe source archive member')
        names = [member.name for member in members]
        if len(set(names)) != len(names):
            raise ValueError('duplicate source archive member')
        if tar.pax_headers.get('comment') != m['source_revision']:
            raise ValueError('source archive commit identity mismatch')
        inventory = json.loads(retained_path(m['inventory']).read_text())
        expected_inventory = {item['path']: item['sha256'] for item in inventory}
        actual = {member.name[len(prefix):]: digest(tar.extractfile(member).read())
                  for member in members if member.isfile()}
        if len(expected_inventory) != len(inventory) or actual != expected_inventory:
            raise ValueError('source inventory mismatch')
        lock_digest = digest(tar.extractfile(prefix + 'Cargo.lock').read())
        workspace = tomllib.loads(tar.extractfile(prefix + 'Cargo.toml').read().decode())
        if workspace['workspace']['package']['version'] != m['version']:
            raise ValueError('source version mismatch')
    record_path = retained_path(m['build_record'])
    if digest(record_path.read_bytes()) != m['build_record_sha256']:
        raise ValueError('build record digest mismatch')
    record = json.loads(record_path.read_text())
    if (any(record[key] != m[key] for key in ('version', 'source_revision', 'archive_sha256', 'binary_sha256', 'target_triple'))
            or record['builder']['reference'] != m['builder']
            or record['cargo_lock_sha256'] != lock_digest
            or record['intended_tag'] != m['release_tag']
            or record['intended_release_url'] != m['release_url']
            or record['upstream_git']['version'] != m['git']['version']
            or record['upstream_git']['archive_sha256'] != m['git']['sha256']):
        raise ValueError('build record provenance mismatch')
    recorded_inventory = {item['path_utf8']: item['sha256']
                          for item in record['development_manifest']['source']['files']}
    if recorded_inventory != actual:
        raise ValueError('build record source inventory mismatch')
    git = m['git']
    source = retained_path(git['archive'])
    if digest(source.read_bytes()) != git['sha256']:
        raise ValueError('Git runtime source digest mismatch')
    verify_bundle(m, actual)
    checks = [(m['archive_sha256'], m['archive']), (m['bundle_sha256'], 'aq-git-source.bundle'),
              (m['binary_sha256'], m['binary']), (m['build_record_sha256'], m['build_record']),
              (git['sha256'], git['archive'])]
    expected = ''.join(f'{checksum}  {name}\n' for checksum, name in checks)
    if (ROOT / 'vendor/git-candidate.sha256').read_text() != expected:
        raise ValueError('candidate checksum file mismatch')
    if official:
        verify_build_stages(m)
    return m


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--official', action='store_true')
    args = parser.parse_args()
    print(json.dumps(candidate(args.official), sort_keys=True))
