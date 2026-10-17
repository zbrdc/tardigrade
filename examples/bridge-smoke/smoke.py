#!/usr/bin/env python3
# End-to-end vet for tardigrade-bridge (docs/BRIDGE_CONTRACT.md).
# Usage: python3 smoke.py [SOAK_SECONDS]   (default 240)

import json, os, random, socket, subprocess, sys, threading, time
from http.client import HTTPConnection

DIR = os.path.dirname(os.path.abspath(__file__))
OUT_BIN = os.environ.get('TARDIGRADE_BIN', 'tardigrade')
BRIDGE_BIN = os.environ.get('TARDIGRADE_BRIDGE_BIN', 'tardigrade-bridge')
SOAK = int(sys.argv[1]) if len(sys.argv) > 1 else 240
results = []

def check(name, cond, detail=''):
    results.append((name, bool(cond)))
    print(('PASS ' if cond else 'FAIL ') + name + (('  -- ' + str(detail)[:120]) if detail and not cond else ''), flush=True)

def free_port():
    s = socket.socket(); s.bind(('127.0.0.1', 0)); p = s.getsockname()[1]; s.close(); return p

def start_bridge(workers, queue, flags):
    port = free_port()
    args = [BRIDGE_BIN, f'--workers={workers}', f'--queue-depth={queue}', f'--addr=127.0.0.1:{port}', '--',
            os.path.join(DIR, 'app.js')] + flags
    proc = subprocess.Popen(args, env=dict(os.environ, OUT=OUT_BIN), stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
    line = proc.stdout.readline().strip()
    return proc, int(line.rsplit(':', 1)[1])

def req(port, path, body='', raw=None):
    # body may be str (sent as UTF-8) or bytes; returns (status, text).
    data = body.encode('utf-8') if isinstance(body, str) else body
    c = HTTPConnection('127.0.0.1', port, timeout=20)
    try:
        if raw is not None:
            c.connect(); c.sock.sendall(raw); buf = b''
            while True:
                chunk = c.sock.recv(65536)
                if not chunk: break
                buf += chunk
            head = buf.split(b'\r\n', 1)[0].decode('utf-8', 'replace')
            status = int(head.split()[1]) if len(head.split()) > 1 else 0
            return status, buf.decode('utf-8', 'replace')
        c.request('POST', path, body=data, headers={'Content-Length': str(len(data))})
        r = c.getresponse(); text = r.read().decode('utf-8', 'replace')
        return r.status, text
    finally:
        c.close()

def children(pid):
    out = []
    try:
        for tid in os.listdir(f'/proc/{pid}/task'):
            try:
                with open(f'/proc/{pid}/task/{tid}/children') as f: out += [int(x) for x in f.read().split()]
            except OSError: pass
    except OSError: pass
    return sorted(set(out))

def rss_kb(pid):
    try:
        with open(f'/proc/{pid}/status') as f:
            for line in f:
                if line.startswith('VmRSS:'): return int(line.split()[1])
    except OSError: pass
    return -1

def fds(pid):
    try: return len(os.listdir(f'/proc/{pid}/fd'))
    except OSError: return -1

backend = subprocess.Popen(['python3', os.path.join(DIR, 'backend.py'), str(free_port())])
time.sleep(0.3)
bport = int(open(f'/proc/{backend.pid}/cmdline', 'rb').read().split(b'\0')[-2])

NET = [f'--allow-call=backend=http://127.0.0.1:{bport}/echo,POST', '--net-allow-private', '--net-timeout=3000', '--sandbox']

try:
    a, aport = start_bridge(1, 0, NET)
    time.sleep(0.5)
    s, t = req(aport, '/'); check('root hello', s == 200 and t == 'hello from RAM', (s, t))
    h1 = json.loads(req(aport, '/state')[1])['hits']; h2 = json.loads(req(aport, '/state')[1])['hits']
    check('state counter increments', h2 == h1 + 1, (h1, h2))
    s, t = req(aport, '/fs/write', 'smoke-payload-42'); check('fs write', s == 200 and t == 'ok', (s, t))
    s, t = req(aport, '/fs/read'); check('fs read back', s == 200 and t == 'smoke-payload-42', (s, t))
    s, t = req(aport, '/fs/list'); check('fs list has file', s == 200 and 'last.txt' in t, (s, t))
    s, t = req(aport, '/wasm', '19,23'); check('wasm add', s == 200 and t == '42', (s, t))
    s, t = req(aport, '/net/echo', 'ping'); j = json.loads(t) if s == 200 else {}
    check('net capability roundtrip', s == 200 and j.get('up') == 200 and 'ping' in j.get('echoed', ''), (s, t))
    s, t = req(aport, '/encode', 'h\u00e9llo w\u00f6rld'); j = json.loads(t) if s == 200 else {}
    check('text encode/decode roundtrip', s == 200 and j.get('roundtrip') == 'h\u00e9llo w\u00f6rld', (s, t))
    s, t = req(aport, '/clone'); j = json.loads(t) if s == 200 else {}
    check('structuredClone deep copy', s == 200 and j['orig']['nested']['ok'] is True and j['copy']['nested']['ok'] is False, (s, t))
    s, t = req(aport, '/cpu'); check('cpu work under sandbox', s == 200 and t.startswith('cpu '), (s, t))
    s, t = req(aport, '/sleep', '150'); check('timers', s == 200 and t == 'slept 150', (s, t))
    s, t = req(aport, '/boom'); check('guest error maps to 500', s == 500 and 'intentional' in t, (s, t))
    s, t = req(aport, '/', raw=b'GARBAGE\r\n\r\n'); check('garbage line rejected', s == 400, (s, t[:40]))
    s, t = req(aport, '/', raw=b'POST / HTTP/1.1\r\nHost: t\r\nContent-Length: 99999999\r\n\r\n'); check('oversize declared rejected', s == 413, (s, t[:40]))
    s, t = req(aport, '/die'); check('dead worker maps to 502', s == 502, (s, t))
    time.sleep(0.5)
    s, t = req(aport, '/state'); j = json.loads(t) if s == 200 else {}
    check('worker restart resets state', s == 200 and j['hits'] <= 2 and j['bootAgoMs'] < 5000, (s, t))
    s, t = req(aport, '/fs/read'); check('fs state lost on restart (documented)', s == 500 and 'no such file' in t, (s, t))
    outs = []
    def slow(): outs.append(req(aport, '/sleep', '400')[0])
    th = [threading.Thread(target=slow) for _ in range(2)]
    [x.start() for x in th]; [x.join() for x in th]
    check('overload is a defined 503', sorted(outs) == [200, 503], outs)
    a.terminate(); a.wait(timeout=10)

    b, bport2 = start_bridge(2, 8, NET)
    time.sleep(0.5)
    t0 = time.time(); outs = []
    def one(): outs.append(req(bport2, '/sleep', '300')[0])
    th = [threading.Thread(target=one) for _ in range(8)]
    [x.start() for x in th]; [x.join() for x in th]
    dt = time.time() - t0
    check('8 parallel requests all served', sorted(outs) == [200]*8, outs)
    check('two workers overlap (elapsed < 1.9s)', dt < 1.9, dt)

    print(f'SOAK {SOAK}s begin', flush=True)
    mix = ['/state', '/cpu', '/encode', '/net/echo', '/wasm', '/']
    ok = err = 0; samples = []; start = time.time(); next_sample = 0
    while time.time() - start < SOAK:
        try:
            p = random.choice(mix)
            body = random.choice(['x', 'hello world', '19,23'])
            s, t = req(bport2, p, body)
            if s == 200: ok += 1
            else: err += 1; print('SOAK-ERR', p, s, t[:80], flush=True)
        except Exception as e:
            err += 1; print('SOAK-EXC', repr(e)[:120], flush=True)
        elapsed = time.time() - start
        if elapsed >= next_sample:
            kids = children(b.pid)
            zombies = 0
            for k in kids:
                try:
                    with open(f'/proc/{k}/status') as f: zombies += ('Zombie' in f.read())
                except OSError: pass
            samples.append((int(elapsed), rss_kb(b.pid), [rss_kb(k) for k in kids], len(kids), zombies, fds(b.pid)))
            print(f'SOAK t+{int(elapsed)}s bridgeRSS={samples[-1][1]}kB workers={len(kids)} workerRSS={samples[-1][2]} zombies={zombies} bridgeFDs={samples[-1][5]}', flush=True)
            next_sample += 30
        time.sleep(0.15)
    check(f'soak: >{SOAK * 4} successes', ok > SOAK * 4, ok)
    check('soak: zero errors', err == 0, err)
    kids = children(b.pid)
    check('soak: still exactly 2 workers', len(kids) == 2, kids)
    if len(samples) >= 2:
        print(f'SOAK bridge RSS drift: {samples[-1][1] - samples[0][1]} kB over {samples[-1][0] - samples[0][0]}s', flush=True)
    b.terminate(); b.wait(timeout=10)
finally:
    backend.terminate(); backend.wait(timeout=5)

failed = [n for n, ok in results if not ok]
print(f'\nSMOKE RESULT: {len(results)-len(failed)}/{len(results)} passed', flush=True)
if failed: print('FAILED: ' + ', '.join(failed), flush=True)
sys.exit(1 if failed else 0)
