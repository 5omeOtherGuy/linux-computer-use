#!/usr/bin/env python3
"""Link the bundled desktop commands and skill; no packages or panel changes."""
import argparse
from pathlib import Path


def install(bin_dir, skills_dir, replace_links=False):
    desktop = Path(__file__).resolve().parents[1] / 'desktop'
    skill = skills_dir / 'agent-desktop'
    links = {
        bin_dir / 'agent-desktop': desktop / 'bin/agent-desktop',
        bin_dir / 'agent-desktop-menu': desktop / 'panel/agent-desktop-menu',
        bin_dir / 'agent-desktop-genmon': desktop / 'panel/agent-desktop-genmon',
        skill / 'SKILL.md': desktop / 'skill/SKILL.md',
        skill / 'references': desktop / 'skill/references',
    }
    # Check every destination before changing any links.
    for destination, source in links.items():
        if not source.exists():
            raise SystemExit(f'missing bundled source: {source}')
        if destination.is_symlink():
            if destination.resolve() != source.resolve() and not replace_links:
                raise SystemExit(f'existing link: {destination}; use --replace-links to migrate')
        elif destination.exists():
            raise SystemExit(f'refusing to replace a regular file/directory: {destination}')
    for destination, source in links.items():
        destination.parent.mkdir(parents=True, exist_ok=True)
        if destination.is_symlink():
            if destination.resolve() == source.resolve():
                continue
            destination.unlink()
        destination.symlink_to(source, target_is_directory=source.is_dir())
        print(f'{destination} -> {source}')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir', type=Path, default=Path.home() / '.local/bin')
    parser.add_argument('--skills-dir', type=Path, default=Path.home() / '.agents/skills')
    parser.add_argument('--replace-links', action='store_true', help='migrate existing symlinks; regular files are always refused')
    args = parser.parse_args()
    install(args.bin_dir.expanduser().absolute(), args.skills_dir.expanduser().absolute(), args.replace_links)
