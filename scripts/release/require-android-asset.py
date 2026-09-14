#!/usr/bin/env python3
"""Post-publication release gate: workflow status cannot prove asset attachment."""
import argparse
import json
import re
import subprocess
import sys


def gh_json(*arguments):
    return json.loads(subprocess.check_output(['gh', *arguments], text=True))


def require_assets(release, tag):
    if not isinstance(release, dict):
        raise ValueError('invalid release response')
    if release.get('tagName') != tag or release.get('isDraft') is not False:
        raise ValueError('release is missing, draft, or has the wrong tag')
    assets = release.get('assets')
    if not isinstance(assets, list) or not all(isinstance(asset, dict) for asset in assets):
        raise ValueError('invalid release assets response')
    required = [f'haider-{tag}-android.apk', f'haider-{tag}-android.apk.sha256']
    attached = []
    for name in required:
        matches = [asset for asset in assets if asset.get('name') == name]
        if len(matches) != 1:
            raise ValueError(f'expected exactly one attached asset: {name}; found {len(matches)}')
        asset = matches[0]
        if asset.get('state') != 'uploaded' or not isinstance(asset.get('size'), int) or asset['size'] <= 0:
            raise ValueError(f'asset is empty or not uploaded: {name}')
        attached.append(asset)
    return attached


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('tag')
    parser.add_argument('sha', help='expected full peeled tag commit')
    parser.add_argument('--repo', required=True)
    args = parser.parse_args()
    if not re.fullmatch(r'v[0-9]+\.[0-9]+\.[0-9]+', args.tag):
        parser.error('expected a numeric vMAJOR.MINOR.PATCH tag')
    if not re.fullmatch(r'[0-9a-f]{40}', args.sha):
        parser.error('expected full commit SHA')
    if not re.fullmatch(r'[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+', args.repo):
        parser.error('expected owner/repo')
    try:
        # GitHub resolves annotated tags to their commit here; targetCommitish
        # on a release may just say "main" and is not candidate evidence.
        commit = gh_json('api', f'repos/{args.repo}/commits/{args.tag}')
        if not isinstance(commit, dict) or commit.get('sha') != args.sha:
            raise ValueError('remote tag does not peel to the expected release commit')
        release = gh_json('release', 'view', args.tag, '--repo', args.repo,
                          '--json', 'tagName,isDraft,url,assets')
        assets = require_assets(release, args.tag)
        print(json.dumps(dict(verdict='PASS_ANDROID_RELEASE_ASSETS', tag=args.tag, sha=args.sha,
                              release=release.get('url'), assets=assets), indent=2))
        return 0
    except (ValueError, KeyError, TypeError, OSError, subprocess.CalledProcessError) as error:
        print(f'::error::RELEASE INCOMPLETE: Android APK gate failed for {args.tag}: {error}', file=sys.stderr)
        return 1


if __name__ == '__main__':
    sys.exit(main())
