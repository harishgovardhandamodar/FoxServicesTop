#!/usr/bin/env python3
"""Checks the direct production dependencies between fox-services-top's workspace crates.

The rules, for the crate list D-79 settled with the state, history and
compression components:

* every implementation crate depends on no workspace crate but `fox-services-top-core`,
  except that `fox-services-top-history` may use the pure `fox-services-top-compression`;
* only the composition root `fox-services-top` depends on more;
* crates that must stay pure or free of drivers forbid the external crates
  listed in PURE and FORBIDDEN below.

It checks direct production edges only (dev dependencies are reviewed
separately), not trait permissions or std::fs use in code.
"""
import argparse
import json
from pathlib import Path

NAMES = {
    'fox-services-top-core', 'fox-services-top-runtime', 'fox-services-top-adapters', 'fox-services-top-collectors',
    'fox-services-top-state', 'fox-services-top-history', 'fox-services-top-compression', 'fox-services-top-store',
    'fox-services-top-manager', 'fox-services-top-ui', 'fox-services-top',
}
PURE = {'libc', 'nix', 'nvml-wrapper', 'rusqlite', 'ratatui', 'crossterm', 'tokio'}
FORBIDDEN = {
    'fox-services-top-core': PURE,
    'fox-services-top-compression': PURE,
    'fox-services-top-state': PURE,
    'fox-services-top-history': PURE,
    'fox-services-top-collectors': PURE - {'tokio'},
    'fox-services-top-manager': PURE - {'tokio'},
    'fox-services-top-ui': {'libc', 'nix', 'nvml-wrapper', 'rusqlite'},
}
# Workspace edges allowed beside fox-services-top-core.
ALLOWED = {
    'fox-services-top-history': {'fox-services-top-compression'},
}


def check(metadata):
    errors = []
    members = set(metadata.get('workspace_members', []))
    packages = [package for package in metadata['packages'] if package.get('id') in members]
    seen = {package['name'] for package in packages}
    if seen != NAMES:
        errors.append(f'workspace names mismatch missing={sorted(NAMES - seen)} extra={sorted(seen - NAMES)}')
    for package in packages:
        name = package['name']
        for dependency in package.get('dependencies', []):
            if dependency.get('kind') == 'dev':
                continue
            target = dependency['name']
            allowed = {'fox-services-top-core'} | ALLOWED.get(name, set())
            if target in NAMES and name != 'fox-services-top' and target not in allowed:
                errors.append(f'{name} must not depend on {target}')
            if target in FORBIDDEN.get(name, set()):
                errors.append(f'{name} must not depend on {target}')
    return errors


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('metadata', type=Path, help='output of cargo metadata --format-version 1 --locked')
    args = parser.parse_args()
    try:
        errors = check(json.loads(args.metadata.read_text()))
    except (OSError, ValueError, KeyError, TypeError) as error:
        parser.error(str(error))
    print(json.dumps({'status': 'FAIL' if errors else 'PASS', 'errors': errors}, indent=2))
    return int(bool(errors))


if __name__ == '__main__':
    raise SystemExit(main())
