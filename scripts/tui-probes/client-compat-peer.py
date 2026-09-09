#!/usr/bin/env python3
"""Fixture UDS peer for the real TUI compatibility/cursor regression.

Uses only a throwaway profile; never opens a vault or calls a provider. Requests
are logged by method and cursor, without their content. `release` in --control
lets the operator hold replay long enough to inspect the resyncing state.
"""
import argparse
from collections import Counter
import json
import os
from pathlib import Path
import socket
import struct
import threading
import traceback

import blake3

p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--profile', type=Path, required=True)
p.add_argument('--runtime', type=Path, required=True)
p.add_argument('--control', type=Path, required=True)
p.add_argument('--journal', type=Path)
p.add_argument('--scenario', choices=['monitor', 'gap', 'lagged', 'malformed', 'mismatch'], default='gap')
p.add_argument('--hold-replay', action='store_true')
a = p.parse_args()
a.profile.mkdir(parents=True, exist_ok=True)
a.control.mkdir(parents=True, exist_ok=True)
profile_id = blake3.blake3(b'haider-profile-id-v1\n' + str(a.profile.resolve()).encode()).hexdigest()
scope = a.runtime / profile_id[:20]
scope.mkdir(parents=True, mode=0o700, exist_ok=True)
os.chmod(a.runtime, 0o700)
os.chmod(scope, 0o700)
endpoint = scope / 'h.sock'
assert not endpoint.exists(), 'refuse to overwrite any socket'
SESSION = 'compat-session'


def log(**fields):
    print(json.dumps(fields, ensure_ascii=False), flush=True)


def envelope(seq, payload, ui=False):
    return dict(schema_version=1, event_id=f'fixture-{seq}', seq=seq,
                session_id=SESSION, device_id='fixture', authority_epoch=1,
                worker_generation=1, committed_at_ms=seq,
                render=dict(ui=ui, durable=True, prompt='omit'), payload=payload)


if a.journal:
    journal = [json.loads(line) for line in a.journal.read_text().splitlines() if line]
    # The supplied journal is an explicit sanitized fixture, never a profile.
    for row in journal:
        row['session_id'] = SESSION
else:
    fixtures = Path(__file__).resolve().parents[2] / 'crates/haider-tui/tests/fixtures/compat_additive.json'
    payloads = json.loads(fixtures.read_text())
    protocol_fixtures = fixtures.parents[3] / 'haider-protocol/tests/fixtures'
    for name in ('task_started.json', 'task_completed.json', 'usage_account_tagged.json',
                 'agent_messaged.json', 'agent_metrics_snapshot.json',
                 'convergence_graph_facts.json', 'convergence_graph_m2a_authority.json'):
        payload = json.loads((protocol_fixtures / name).read_text())
        if name == 'usage_account_tagged.json':
            payload['type'] = 'usage'
        payloads.extend(payload if isinstance(payload, list) else [payload])
    monitor = [{'type': tag} for tag in ('monitor_report_delivered', 'monitor_removed', 'monitor_report_pending')]
    journal = [envelope(1, dict(type='user_message', text='Compatibility reproduction', attachments=[]), True)]
    journal += [envelope(len(journal) + i + 1, payload) for i, payload in enumerate(monitor + payloads)]
    journal.append(envelope(len(journal) + 1, dict(type='user_message', text='Replay complete — cursor recovered', attachments=[]), True))
head = journal[-1]['seq']
features = ['context_compaction_v1', 'session_mutation_v1', 'turn_control_v1', 'provider_configure_v1', 'account_oauth_pkce_v1']


def receive(conn, n):
    data = bytearray()
    while len(data) < n:
        part = conn.recv(n - len(data))
        if not part:
            raise EOFError
        data.extend(part)
    return data


def serve(conn):
    lock = threading.Lock()
    attachments = set()
    attach_count = 0

    def send(frame):
        data = json.dumps({'v': 1, **frame}, separators=(',', ':')).encode()
        with lock:
            conn.sendall(struct.pack('>I', len(data)) + data)

    def replay(attachment, after, attempt):
        try:
            sent_types = Counter()
            if a.hold_replay and attempt > 1:
                log(state='replay_held', attachment=attachment, after_seq=after)
                while not (a.control / 'release').exists() and attachment in attachments:
                    threading.Event().wait(.1)
            for row in journal:
                if row['seq'] <= after or attachment not in attachments:
                    continue
                if a.scenario == 'malformed' and row['seq'] == 2 and not (a.control / 'healthy').exists():
                    row = {**row, 'payload': {'type': 42}}
                # A missing middle event is detected by the next sequence.
                if a.scenario == 'gap' and attempt == 1 and row['seq'] == 2:
                    continue
                if a.scenario == 'lagged' and attempt == 1 and row['seq'] == 2:
                    send(dict(kind='lagged', attachment_id=attachment, last_queued_seq=head))
                    attachments.discard(attachment)
                    log(state='lagged', last_queued_seq=head)
                    return
                send(dict(kind='event', attachment_id=attachment, session_id=SESSION, envelope=row))
                kind = row['payload'].get('type')
                sent_types[kind if isinstance(kind, str) else '<malformed>'] += 1
            if attachment in attachments:
                send(dict(kind='attach_caught_up', attachment_id=attachment, high_water_seq=head))
                log(state='caught_up', attachment=attachment, high_water_seq=head, sent_types=sent_types)
        except (BrokenPipeError, OSError):
            pass

    try:
        while True:
            size = struct.unpack('>I', receive(conn, 4))[0]
            assert 0 < size <= 16 * 1024 * 1024
            frame = json.loads(receive(conn, size))
            kind = frame['kind']
            if kind == 'hello':
                client = frame.get('client_version', '')
                daemon = '0.0.999' if a.scenario == 'mismatch' and not (a.control / 'healthy').exists() else client
                log(state='hello', client=client, daemon=daemon, protocol_min=frame.get('protocol_min'), protocol_max=frame.get('protocol_max'))
                send(dict(kind='welcome', protocol=1, instance_id='compat-fixture', daemon_generation=99,
                          frame_limit=16 * 1024 * 1024, profile_id=profile_id, daemon_version=daemon,
                          lifecycle_phase='ready', capabilities_granted=frame.get('capabilities_requested', []), features=features))
            elif kind == 'ping':
                send(dict(kind='pong', nonce=frame['nonce']))
            elif kind == 'request':
                body = frame['body']
                method = body['method']
                log(method=method, **{k: body[k] for k in ('after_seq', 'attachment_id') if k in body})
                response = dict(method=method)
                if method == 'session.list':
                    response.update(sessions=[dict(session_id=SESSION, head_seq=head, worker_generation=1, title='Compatibility probe', provider='fake', last_model='fixture')])
                elif method == 'session.attach':
                    attach_count += 1
                    attachment = f'fixture-attachment-{attach_count}'
                    attachments.add(attachment)
                    after = body['after_seq']
                    response.update(attachment_id=attachment, attach_state=dict(session_id=SESSION, requested_after_seq=after, replay_through_seq=head, worker_generation=1, authority_epoch=1))
                elif method == 'session.detach':
                    attachments.discard(body['attachment_id'])
                    response.update(attachment_id=body['attachment_id'])
                elif method == 'session.diagnostic':
                    log(state='FALSE_LATCH' if a.scenario != 'mismatch' else 'diagnostic', code=body.get('code'))
                    response.update(recorded_seq=head)
                elif method == 'status.snapshot':
                    response.update(session_count=1, ready=True, ready_since=1, providers_loaded=True)
                elif method == 'account.list':
                    response.update(descriptors=[], revision=1)
                elif method == 'provider.list':
                    response.update(providers=[], revision=1)
                elif method == 'loom.list':
                    response.update(agent_types=[], workflows=[], workflow_catalog=[])
                elif method == 'command.list':
                    response.update(items=[])
                elif method in ('session.list_watch', 'account.list_watch'):
                    response.update(accepted=True)
                elif method == 'session.read':
                    bounds = body['range']
                    response.update(result=dict(session_id=SESSION, range=bounds, head_seq=head, envelopes=[r for r in journal if bounds['start_seq'] <= r['seq'] <= bounds['end_seq']]))
                else:
                    response = dict(method='error', code='not_found', message='fixture does not implement ' + method, retryable=False)
                send(dict(kind='response', request_id=frame['request_id'], body=response))
                if method == 'session.attach':
                    threading.Thread(target=replay, args=(attachment, after, attach_count), daemon=True).start()
    except (EOFError, ConnectionResetError, BrokenPipeError):
        pass
    except Exception:
        traceback.print_exc()
    finally:
        attachments.clear()
        conn.close()


listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
listener.bind(str(endpoint))
os.chmod(endpoint, 0o600)
listener.listen()
(a.control / 'ready.json').write_text(json.dumps(dict(pid=os.getpid(), profile=str(a.profile), runtime=str(a.runtime), endpoint=str(endpoint), head=head)))
log(state='ready', endpoint=str(endpoint), head=head)
try:
    while True:
        conn, _ = listener.accept()
        threading.Thread(target=serve, args=(conn,), daemon=True).start()
finally:
    listener.close()
    endpoint.unlink(missing_ok=True)
