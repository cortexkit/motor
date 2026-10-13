#!/usr/bin/env python3
"""Operator-only break tests that weaken isolation to prove the guards can fail.

Each temporary edit is marked NON-VACUITY BREAK. On dedicated trusted test
infrastructure with no other jobs, remove motor_net's TCP-reset and ICMP input
rejects, reload ONLY that table, and require isolated_net_input_rejects_host_and_gateway
to fail while the public-network test passes. Then restore/reload in finally.
The second control changes backend.rs to grant every isolated job networking,
requires isolated_net_offline_job_control to fail, then restores backend.rs.
Remote runon jobs lack host privileges; never run this through runon or in production.
"""
import argparse
import json
import os
import pathlib
import re
import subprocess
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[2]


def run(*args, check=True, env=None, timeout=None):
    return subprocess.run(args, cwd=ROOT, env=env, text=True, stdout=subprocess.PIPE,
                          stderr=subprocess.STDOUT, check=check, timeout=timeout)


def nft_load(path):
    text = 'delete table inet motor_net\n' + path.read_text()
    with tempfile.NamedTemporaryFile(mode='w', prefix='motor-net-control-', suffix='.nft') as file:
        file.write(text)
        file.flush()
        run('/usr/bin/sudo', '-n', '/usr/sbin/nft', '--check', '--file', file.name)
        run('/usr/bin/sudo', '-n', '/usr/sbin/nft', '--file', file.name)


PUBLIC_CONTROL = 'isolation::tests::isolated_net_public_https_dns_read_only_resolver'


def test_results(output):
    # nextest repeats failures in its final list. Grade distinct test names,
    # not printed rows, and require the independent public control to run.
    output = re.sub(r'\x1b\[[0-?]*[ -/]*[@-~]', '', output)
    names = {'PASS': set(), 'FAIL': set(), 'TIMEOUT': set()}
    rows = []
    seen = set()
    for line in output.splitlines():
        match = re.match(r'^\s*(PASS|FAIL|TIMEOUT)\s+\[[^]]+\].*?\s+(\S+)\s*$', line)
        if match:
            status, name = match.groups()
            names[status].add(name)
            if (status, name) not in seen:
                seen.add((status, name))
                rows.append(line.strip())
        elif 'Summary [' in line:
            rows.append(line.strip())
    return names, '\n'.join(rows)[:400]


def grade(output, expected, returncode):
    names, captured = test_results(output)
    if names['TIMEOUT']:
        outcome = 'hung'
    elif returncode != 0 and names['FAIL'] == {expected} and names['PASS'] == {PUBLIC_CONTROL}:
        outcome = 'reddened'
    elif returncode == 0 and names['PASS'] == {expected, PUBLIC_CONTROL} and not names['FAIL']:
        outcome = 'undefended'
    else:
        outcome = 'not_reached'
    return outcome, captured


def control(result, path, old, new, expected):
    relative = str(path.relative_to(ROOT))
    before = during = after = ''
    output = ''
    captured = ''
    outcome = 'not_reached'
    staged = restored = False
    try:
        run('git', 'add', '--', relative)
        staged = True
        before = run('git', 'diff', '--stat').stdout
        assert not before, f'unstaged changes before mutation: {before}'
        original = path.read_text()
        assert original.count(old) == 1, 'mutation anchor changed'
        path.write_text(original.replace(old, new))
        during = run('git', 'diff', '--stat').stdout
        assert during, 'NON-VACUITY BREAK did not change a file'
        if path.suffix == '.nft':
            nft_load(path)
        env = dict(os.environ)
        for key in ['GIT_CONFIG_COUNT', 'GIT_CONFIG_KEY_0', 'GIT_CONFIG_VALUE_0']:
            env.pop(key, None)
        expression = f'test(={expected}) | test(={PUBLIC_CONTROL})'
        completed = run('cargo', 'nextest', 'run', '-p', 'motor-runner', '--lib', '--locked',
                        '--run-ignored', 'only', '--no-fail-fast', '-E', expression,
                        '--test-threads', '1', '--no-capture', check=False, env=env, timeout=360)
        output = completed.stdout
        outcome, captured = grade(output, expected, completed.returncode)
    except subprocess.TimeoutExpired as error:
        output = error.stdout or ''
        if isinstance(output, bytes):
            output = output.decode(errors='replace')
        outcome = 'hung'
        captured = 'control command exceeded 360 seconds'
    except Exception as error:
        output += f'\ncontrol error: {error}\n'
        captured = str(error)[:400]
    finally:
        try:
            if staged:
                run('git', 'checkout', '--', relative)
                path.touch()
                if path.suffix == '.nft':
                    nft_load(path)
            after = run('git', 'diff', '--stat').stdout
            assert not after, f'mutation was not fully restored: {after}'
            restored = True
        except Exception as error:
            output += f'\nRESTORE FAILED: {error}\n'
            captured = f'RESTORE FAILED: {error}'[:400]
            outcome = 'not_reached'
        (result / (path.stem + '-red.log')).write_text(output)
        evidence = {'control': f'NON-VACUITY BREAK: {relative}', 'expected_red': expected,
                    'captured_output': captured,
                    'applied_evidence': f'{relative}: before={before.strip()!r}; during={during.strip()!r}; restored={after.strip()!r}',
                    'outcome': outcome}
        (result / (path.stem + '-evidence.json')).write_text(json.dumps(evidence, indent=2) + '\n')
        print(json.dumps(evidence))
    return evidence, restored


def run_controls(result):
    plans = [
        (ROOT / 'script/runner/motor-net.nft',
         '        iifname "mnet-*" meta l4proto tcp reject with tcp reset\n        iifname "mnet-*" reject with icmpx type port-unreachable\n    }\n    chain forward',
         '        # NON-VACUITY BREAK: host-input rejects removed\n    }\n    chain forward',
         'isolation::tests::isolated_net_input_rejects_host_and_gateway'),
        (ROOT / 'crates/motor-runner/src/backend.rs',
         'if r.network == Some(Network::Outbound) {',
         'if config.job_network.is_some() { // NON-VACUITY BREAK: grant every isolated job',
         'isolation::tests::isolated_net_offline_job_control'),
    ]
    evidence = []
    safe = True
    for path, old, new, expected in plans:
        if safe:
            record, safe = control(result, path, old, new, expected)
        else:
            # Never weaken another guard after restoration failed. Still write
            # the second record, so missing evidence cannot look like a pass.
            record = {'control': f'NON-VACUITY BREAK: {path.relative_to(ROOT)}',
                      'expected_red': expected, 'captured_output': 'not run: preceding control could not be safely restored',
                      'applied_evidence': 'no mutation applied', 'outcome': 'not_reached'}
            (result / (path.stem + '-evidence.json')).write_text(json.dumps(record, indent=2) + '\n')
            print(json.dumps(record))
        evidence.append(record)
    (result / 'mutation-evidence.json').write_text(json.dumps(evidence, indent=2) + '\n')
    return all(record['outcome'] == 'reddened' for record in evidence)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--results', required=True, type=pathlib.Path)
    parser.add_argument('--apply', action='store_true', help='required explicit approval of temporary isolation breaks')
    args = parser.parse_args()
    if not args.apply:
        raise SystemExit('Refusing host mutation without --apply; read the module docstring first')
    result = args.results.resolve()
    assert not any(part in {'target', 'dist', 'node_modules', '.cache'} for part in result.parts)
    result.mkdir(parents=True, exist_ok=True)
    assert not run('git', 'status', '--porcelain').stdout, 'run controls only from a clean committed worktree'
    if not run_controls(result):
        raise SystemExit('one or more mutation controls failed qualification; inspect both evidence records')


if __name__ == '__main__':
    main()
