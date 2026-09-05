import copy, json, re
from pathlib import Path
E = Path(__file__).resolve().parent
BEFORE = Path((E / 'before-root.txt').read_text().strip())

def parse(line):
    return json.loads(re.sub(r':<[A-Z0-9_]+>', ':0', line))

for old_file in sorted((BEFORE / 'crates/haider-cli/tests/fixtures').rglob('*.jsonl')):
    name = old_file.relative_to(BEFORE)
    old = []
    side = 'common'
    for line in old_file.read_text().splitlines():
        if line.startswith('<<<<<<<'): side = 'head'
        elif line == '=======': side = 'incoming'
        elif line.startswith('>>>>>>>'): side = 'common'
        elif side in ('common', 'incoming'): old.append(line)
    new = Path(name).read_text().splitlines()
    print(name)
    consumed = inserted = shifted_items = changed = 0
    receipt = None
    revision_map = {}
    for number, line in enumerate(new, 1):
        value = parse(line)
        payload = value.get('payload', {})
        if payload.get('item', {}).get('kind') == 'turn_workspace_before_v1':
            assert value['render'] == {'ui': False, 'durable': True, 'prompt': 'omit'}
            request = payload['provider_request']
            assert request['request_kind'] == 'primary' and request['request_ordinal'] == 1
            assert request['session_id'] == value['session_id'] and request['run_id'] == value['run_id']
            data = payload['item']['data']
            assert data['part'] == 0 and data['parts'] == 1
            assert 'Captured' in data['bytes']
            if payload['event'] == 'started':
                assert receipt is None
                receipt = value
                shifted_items += 1
            else:
                assert payload['event'] == 'completed' and receipt is not None
                assert value['seq'] == receipt['seq'] + 1
                assert receipt['payload']['item'] == payload['item']
                assert receipt['payload']['item_id'] == payload['item_id']
                receipt = None
            print(f'  line {number}: added durable workspace-before receipt {payload["event"]}')
            inserted += 1
            changed += 1
            continue
        assert receipt is None
        previous = parse(old[consumed])
        previous_line = old[consumed]
        consumed += 1
        candidate = copy.deepcopy(value)
        if 'seq' in candidate: candidate['seq'] -= inserted
        event_id = candidate.get('event_id', '')
        if event_id.startswith('worker-event-'):
            stem, suffix = event_id.rsplit('-', 1)
            candidate['event_id'] = stem + '-' + str(int(suffix) - inserted)
        if 'item_id' in payload:
            before = previous['payload']['item_id']
            after = payload['item_id']
            if before != after:
                old_stem, old_suffix = before.rsplit('-', 1)
                new_stem, new_suffix = after.rsplit('-', 1)
                assert old_stem == new_stem and int(new_suffix) - int(old_suffix) == shifted_items
            candidate['payload']['item_id'] = before
        mutation = candidate.get('payload', {}).get('workspace_mutation')
        revision_note = ''
        if mutation and 'workspace_revision' in mutation:
            expected_revision = previous['payload']['workspace_mutation']['workspace_revision']
            before_revision = int(expected_revision.rsplit(':', 1)[1])
            after_revision = int(mutation['workspace_revision'].rsplit(':', 1)[1])
            assert after_revision == value['seq'] and before_revision == previous['seq']
            assert after_revision - before_revision == inserted
            revision_map[mutation['workspace_revision']] = expected_revision
            mutation['workspace_revision'] = expected_revision
            revision_note = f'; outcome-seq-derived workspace revision {before_revision}->{after_revision}'
        if 'workspace_revision' in candidate.get('payload', {}):
            after_revision = candidate['payload']['workspace_revision']
            before_revision = revision_map[after_revision]
            assert previous['payload']['workspace_revision'] == before_revision
            candidate['payload']['workspace_revision'] = before_revision
            revision_note = f'; refers to same outcome revision {before_revision}->{after_revision}'
        def mapped_revision_refs(obj):
            if isinstance(obj, dict):
                return {k: mapped_revision_refs(v) for k, v in obj.items()}
            if isinstance(obj, list):
                return [mapped_revision_refs(v) for v in obj]
            if isinstance(obj, str):
                return re.sub(r'workspace-revision:\d+', lambda m: revision_map.get(m[0], m[0]), obj)
            return obj
        mapped = mapped_revision_refs(candidate)
        if mapped != candidate:
            revision_note += '; embedded references use the same verified outcome revision'
        candidate = mapped
        assert candidate == previous, f'unexpected semantic difference at {name}:{number}: {candidate} != {previous}'
        if line != previous_line:
            print(f'  incoming {consumed} -> line {number}: receipt-induced seq/event offset +{inserted}, item offset +{shifted_items}{revision_note}; all other semantic fields unchanged')
            changed += 1
    assert consumed == len(old) and inserted == 2 and receipt is None
    assert [parse(x)['seq'] for x in new[1:]] == list(range(1, len(new)))
    print(f'  REVIEWED: {changed} changed/new lines vs incoming; one added receipt pair; journalview fields unchanged; contiguous seq.\n')
