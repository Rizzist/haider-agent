import copy, json, re
from pathlib import Path

paths = [
    'crates/haider-cli/tests/fixtures/turnhygiene/run_jsonl_text_turn.jsonl',
    'crates/haider-cli/tests/fixtures/turnhygiene/run_jsonl_tool_turn.jsonl',
    'crates/haider-cli/tests/fixtures/oneshot_run_golden.jsonl',
]
E = Path(__file__).resolve().parent

def conflict_side(name, wanted):
    result = []
    state = 'common'
    for line in (Path((E / 'before-root.txt').read_text().strip()) / name).read_text().splitlines():
        if line.startswith('<<<<<<<'):
            state = 'head'
        elif line == '=======':
            state = 'incoming'
        elif line.startswith('>>>>>>>'):
            state = 'common'
        elif state in ('common', wanted):
            result.append(line)
    return result

def parse(line):
    return json.loads(re.sub(r':<[A-Z0-9_]+>', ':0', line))

for name in paths:
    old = conflict_side(name, 'head')
    new = Path(name).read_text().splitlines()
    print(name)
    index = inserted = changed = markers = 0
    item_ids = {}
    pending_marker = None
    for ni, line in enumerate(new, 1):
        value = parse(line)
        payload = value.get('payload', {})
        if payload.get('item', {}).get('kind') == 'provider_round_terminal_v1':
            assert value['render'] == {'ui': False, 'durable': True, 'prompt': 'omit'}
            correlation = payload['provider_request']
            assert correlation['session_id'] == value['session_id']
            assert correlation['run_id'] == value['run_id']
            if payload['event'] == 'started':
                assert pending_marker is None
                assert 'provider_finish_reason' not in payload
                pending_marker = value
                markers += 1
            else:
                assert payload['event'] == 'completed' and pending_marker is not None
                assert value['seq'] == pending_marker['seq'] + 1
                for field in ('item_id', 'item', 'provider_request'):
                    assert payload[field] == pending_marker['payload'][field]
                assert payload['provider_finish_reason'] == payload['item']['data']['reason']
                pending_marker = None
            print(f'  new line {ni}: atomic terminal {payload["event"]}, request {correlation["request_ordinal"]}, reason={payload["item"]["data"]["reason"]}')
            inserted += 1
            changed += 1
            continue
        assert pending_marker is None, 'terminal pair must be adjacent'
        previous = parse(old[index])
        index += 1
        if line == old[index - 1]:
            continue
        candidate = copy.deepcopy(value)
        added = []
        for key in ('provider_request', 'provider_finish_reason'):
            if key in candidate.get('payload', {}):
                added.append(key)
                del candidate['payload'][key]
        if 'seq' in candidate:
            candidate['seq'] -= inserted
        event_id = candidate.get('event_id', '')
        if event_id.startswith('worker-event-'):
            stem, number = event_id.rsplit('-', 1)
            candidate['event_id'] = stem + '-' + str(int(number) - inserted)
        item_note = ''
        if 'item_id' in payload:
            before = previous['payload']['item_id']
            after = payload['item_id']
            if before not in item_ids:
                if before != after:
                    old_stem, old_number = before.rsplit('-', 1)
                    new_stem, new_number = after.rsplit('-', 1)
                    assert old_stem == new_stem
                    assert int(new_number) - int(old_number) == markers
                item_ids[before] = after
            assert item_ids[before] == after
            candidate['payload']['item_id'] = before
            if before != after:
                item_note = f'; item allocation {before.rsplit("-", 1)[1]}->{after.rsplit("-", 1)[1]}'
        assert candidate == previous, f'unexpected change {name} old {index} new {ni}: {candidate} != {previous}'
        if 'provider_request' in payload:
            request = payload['provider_request']
            assert request['session_id'] == value['session_id'] and request['run_id'] == value['run_id']
            assert request['turn_ordinal'] > 0 and request['request_ordinal'] > 0
        changed += 1
        print(f'  old {index} -> new {ni}: additions={",".join(added) or "none"}; seq/event-id offset=+{inserted}{item_note}; all other fields unchanged')
    assert index == len(old) and pending_marker is None
    print(f'  REVIEWED: {changed} changed/new lines; {markers} atomic terminal pairs; no other semantic changes.\n')

name = 'crates/haider-cli/tests/fixtures/turnhygiene/provider_request_no_budget.json'
assert Path(name).read_bytes() == (Path((E / 'before-root.txt').read_text().strip()) / name).read_bytes()
print(name + ': tooling-regenerated, byte-identical to pre-resolution file')
