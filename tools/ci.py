#!/usr/bin/env python3
"""Resolve a build's per-version ordinal from GitHub's workflow run order."""
import argparse
import base64
import json
import os
import pathlib
import re
import time
import urllib.error
import urllib.parse
import urllib.request

from build import channel_metadata, metadata


def base_version(value):
    match = re.fullmatch(r'(\d+\.\d+\.\d+)(?:-(?:beta[1-9]\d*|beta|release))?', value)
    if not match:
        raise ValueError('Invalid module version')
    return match[1]


def beta_ordinal(current, runs, version, version_at):
    # run_attempt is deliberately ignored: rerunning a job retains its original number.
    previous = {run['id']: run for run in runs
                if run['run_number'] < current['run_number']}
    return 1 + sum(version_at(run['head_sha']) == version for run in previous.values())


class GitHub:
    def __init__(self):
        self.api = os.environ.get('GITHUB_API_URL', 'https://api.github.com').rstrip('/')
        self.repository = os.environ['GITHUB_REPOSITORY']
        self.token = os.environ['GITHUB_TOKEN']
        self.versions = {}

    def get(self, path):
        request = urllib.request.Request(self.api + '/repos/' + self.repository + path,
            headers={'Authorization': 'Bearer ' + self.token,
                     'Accept': 'application/vnd.github+json',
                     'X-GitHub-Api-Version': '2022-11-28',
                     'User-Agent': 'charge-guard-ci'})
        for attempt in range(3):
            try:
                with urllib.request.urlopen(request, timeout=30) as response:
                    return json.load(response)
            except urllib.error.HTTPError as error:
                if attempt < 2 and error.code in (429, 500, 502, 503, 504):
                    time.sleep(2 ** attempt)
                    continue
                raise RuntimeError('GitHub version lookup failed: HTTP ' + str(error.code)) from None
            except urllib.error.URLError:
                if attempt == 2:
                    raise RuntimeError('GitHub version lookup failed: network unavailable') from None
                time.sleep(2 ** attempt)

    def version_at(self, sha):
        if sha not in self.versions:
            payload = self.get('/contents/module/module.prop?ref=' + urllib.parse.quote(sha, safe=''))
            if payload.get('encoding') != 'base64':
                raise ValueError('Unexpected module metadata response')
            text = base64.b64decode(payload['content']).decode('utf-8')
            props = dict(line.split('=', 1) for line in text.splitlines() if '=' in line)
            self.versions[sha] = base_version(props['version'])
        return self.versions[sha]

    def beta_number(self, version):
        current = self.get('/actions/runs/' + os.environ['GITHUB_RUN_ID'])
        if self.version_at(current['head_sha']) != version:
            raise ValueError('Checked-out version does not match the workflow run')
        runs = []
        page = 1
        while True:
            batch = self.get('/actions/workflows/' + str(current['workflow_id'])
                             + '/runs?per_page=100&page=' + str(page))['workflow_runs']
            runs.extend(batch)
            if len(batch) < 100:
                break
            page += 1
        return beta_ordinal(current, runs, version, self.version_at)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('channel', choices=['beta', 'release'])
    args = parser.parse_args()
    meta = metadata()
    number = GitHub().beta_number(base_version(meta['version'])) if args.channel == 'beta' else None
    generated = channel_metadata(meta, args.channel, number)
    filename = generated['id'] + '_v' + generated['version'] + '.zip'
    values = {'beta_number': str(number or ''), 'version': generated['version'], 'filename': filename}
    with pathlib.Path(os.environ['GITHUB_OUTPUT']).open('a', encoding='utf-8', newline='\n') as output:
        output.writelines(key + '=' + value + '\n' for key, value in values.items())
    print('Build artifact: ' + filename)


if __name__ == '__main__':
    main()
