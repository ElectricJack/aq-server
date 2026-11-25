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


def verify_match(actual, expected, message):
    if actual != expected:
        raise ValueError(message)


def digest(data):
    return hashlib.sha256(data).hexdigest()


def is_relative_name(name):
    path = Path(name)
    return not path.is_absolute() and '..' not in path.parts


def retained_path(name):
    vendor = ROOT / 'vendor'
    path = vendor / name
    if not is_relative_name(name) or not path.resolve().is_relative_to(vendor.resolve()):
        raise ValueError('unsafe retained input path')
    return path


def verified_file(name, expected_digest, error):
    path = retained_path(name)
    verify_match(digest(path.read_bytes()), expected_digest, error)
    return path


def verify_build_stages(manifest):
    """Require application Dockerfiles to embed the exact canonical builders."""
    canonical = (ROOT / 'scripts/Dockerfile.aq-git').read_text()
    stages = list(re.finditer(r'^FROM ', canonical, re.MULTILINE))
    verify_match(len(stages), 3, 'canonical aq-git recipe must have two builders and one runtime')
    expected = canonical[:stages[2].start()]
    check_builder_inputs(expected, manifest)
    for name in ('Dockerfile', 'Dockerfile.worker'):
        check_embedded_builders(name, expected)


def check_builder_inputs(expected, manifest):
    verify_match(expected.count('FROM ' + manifest['builder']), 2, 'canonical builder disagrees with candidate manifest')
    for key in ('archive_sha256', 'binary_sha256'):
        if manifest[key] not in expected:
            raise ValueError('canonical release checksums disagree with candidate manifest')
    if manifest['git']['sha256'] not in expected:
        raise ValueError('canonical Git checksum disagrees with candidate manifest')


def check_embedded_builders(name, expected):
    begin = '# BEGIN PINNED AQ-GIT BUILD STAGES\n'
    end = '# END PINNED AQ-GIT BUILD STAGES\n'
    content = (ROOT / name).read_text()
    if content.count(begin) != 1 or content.count(end) != 1:
        raise ValueError(f'{name}: exactly one pinned aq-git block required')
    actual = content.split(begin, 1)[1].split(end, 1)[0]
    verify_match(actual, expected, f'{name}: pinned aq-git builders drifted from scripts/Dockerfile.aq-git')


def verify_bundle(manifest, inventory):
    bundle = verified_file('aq-git-source.bundle', manifest['bundle_sha256'], 'Git bundle digest mismatch')
    heads = subprocess.check_output(['git', 'bundle', 'list-heads', str(bundle)], text=True).splitlines()
    if f"{manifest['source_revision']} HEAD" not in heads:
        raise ValueError('bundle commit identity mismatch')
    # Read committed blobs without extracting source paths onto the filesystem.
    with tempfile.TemporaryDirectory(prefix='aq-git-verify-') as temp:
        repo = Path(temp) / 'source.git'
        subprocess.run(['git', 'clone', '--bare', '--quiet', str(bundle), str(repo)], check=True)
        verify_committed_inventory(repo, manifest['source_revision'], inventory)


def verify_committed_inventory(repo, revision, inventory):
    with tempfile.TemporaryFile() as archive:
        subprocess.run(['git', '-C', str(repo), 'archive', revision], stdout=archive, check=True)
        archive.seek(0)
        with tarfile.open(fileobj=archive) as tar:
            files = {}
            for member in tar:
                if member.isfile():
                    files[member.name] = digest(tar.extractfile(member).read())
                elif not member.isdir():
                    raise ValueError('bundle committed source disagrees with archive inventory')
            verify_match(files, inventory, 'bundle committed source disagrees with archive inventory')


def check_archive_member(member, prefix):
    if not is_relative_name(member.name):
        raise ValueError('unsafe source archive member')
    if not (member.isfile() or member.isdir()):
        raise ValueError('unsafe source archive member')
    if not (member.name + '/').startswith(prefix):
        raise ValueError('unsafe source archive member')


def check_archive_identity(tar, members, prefix, revision):
    for member in members:
        check_archive_member(member, prefix)
    names = [member.name for member in members]
    verify_match(len(set(names)), len(names), 'duplicate source archive member')
    verify_match(tar.pax_headers.get('comment'), revision, 'source archive commit identity mismatch')


def archive_inventory(archive, m):
    prefix = m['archive_prefix']
    verify_match(prefix, f"aq-git-v{m['version']}/", 'source archive prefix disagrees with version')
    with tarfile.open(archive) as tar:
        members = tar.getmembers()
        check_archive_identity(tar, members, prefix, m['source_revision'])
        actual = check_source_inventory(tar, members, prefix, m['inventory'])
        lock_digest = digest(tar.extractfile(prefix + 'Cargo.lock').read())
        workspace = tomllib.loads(tar.extractfile(prefix + 'Cargo.toml').read().decode())
        verify_match(workspace['workspace']['package']['version'], m['version'], 'source version mismatch')
    return actual, lock_digest


def check_source_inventory(tar, members, prefix, inventory_name):
    inventory = json.loads(retained_path(inventory_name).read_text())
    expected_inventory = {item['path']: item['sha256'] for item in inventory}
    actual = {member.name[len(prefix):]: digest(tar.extractfile(member).read())
              for member in members if member.isfile()}
    if len(expected_inventory) != len(inventory) or actual != expected_inventory:
        raise ValueError('source inventory mismatch')
    return actual


def build_inputs_match(record, m, lock_digest):
    inputs = (record['builder']['reference'], record['cargo_lock_sha256'],
              record['intended_tag'], record['intended_release_url'],
              record['upstream_git']['version'], record['upstream_git']['archive_sha256'])
    expected = (m['builder'], lock_digest, m['release_tag'], m['release_url'],
                m['git']['version'], m['git']['sha256'])
    return inputs == expected


def verify_build_record(m, actual, lock_digest):
    record_path = verified_file(m['build_record'], m['build_record_sha256'], 'build record digest mismatch')
    record = json.loads(record_path.read_text())
    keys = ('version', 'source_revision', 'archive_sha256', 'binary_sha256', 'target_triple')
    if any(record[key] != m[key] for key in keys) or not build_inputs_match(record, m, lock_digest):
        raise ValueError('build record provenance mismatch')
    recorded_inventory = {item['path_utf8']: item['sha256']
                          for item in record['development_manifest']['source']['files']}
    verify_match(recorded_inventory, actual, 'build record source inventory mismatch')


def verify_checksums(m):
    git = m['git']
    checks = [(m['archive_sha256'], m['archive']), (m['bundle_sha256'], 'aq-git-source.bundle'),
              (m['binary_sha256'], m['binary']), (m['build_record_sha256'], m['build_record']),
              (git['sha256'], git['archive'])]
    expected = ''.join(f'{checksum}  {name}\n' for checksum, name in checks)
    verify_match((ROOT / 'vendor/git-candidate.sha256').read_text(), expected, 'candidate checksum file mismatch')


def candidate(official=False):
    if official and any(os.environ.get(k) for k in ('AQ_GIT_BIN', 'AQ_GIT_SOURCE_DIR')):
        raise ValueError('official builds reject local aq-git overrides')
    m = json.loads((ROOT / 'vendor/git-candidate.json').read_text())
    if not re.fullmatch('[0-9a-f]{40}', m['source_revision']):
        raise ValueError('full revision required')
    verified_file(m['binary'], m['binary_sha256'], 'aq-git binary digest mismatch')
    archive = verified_file(m['archive'], m['archive_sha256'], 'aq-git source digest mismatch')
    actual, lock_digest = archive_inventory(archive, m)
    verify_build_record(m, actual, lock_digest)
    git = m['git']
    verified_file(git['archive'], git['sha256'], 'Git runtime source digest mismatch')
    verify_bundle(m, actual)
    verify_checksums(m)
    if official:
        verify_build_stages(m)
    return m


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--official', action='store_true')
    args = parser.parse_args()
    print(json.dumps(candidate(args.official), sort_keys=True))
