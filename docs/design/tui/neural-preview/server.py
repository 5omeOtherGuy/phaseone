#!/usr/bin/env python3
"""Local preview and frame-pinned feedback. Python standard library only."""
import base64
import binascii
from datetime import datetime, timezone
from http.server import SimpleHTTPRequestHandler, HTTPServer
import json
import math
from pathlib import Path
import argparse
import uuid

ROOT = Path(__file__).resolve().parent
FEEDBACK = ROOT / 'feedback'


class Handler(SimpleHTTPRequestHandler):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=str(ROOT), **kwargs)

    def json_response(self, status, data):
        body = json.dumps(data).encode()
        self.send_response(status)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Cache-Control', 'no-store')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == '/api/comments':
            path = FEEDBACK / 'comments.jsonl'
            rows = [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []
            self.json_response(200, rows)
        else:
            super().do_GET()

    def do_POST(self):
        if self.path != '/api/comments':
            self.json_response(404, {'error': 'Unknown endpoint'})
            return
        # Other websites must not be able to submit notes to this local listener.
        if self.headers.get('Origin') != f'http://{self.headers.get("Host")}':
            self.json_response(403, {'error': 'Same-origin requests only'})
            return
        try:
            size = int(self.headers.get('Content-Length', '0'))
            if not 0 < size <= 3_000_000:
                raise ValueError('Invalid request size')
            data = json.loads(self.rfile.read(size))
            text = data['text']
            if not isinstance(text, str) or not text.strip() or len(text) > 4000:
                raise ValueError('Invalid note')
            for key, limit in [('x', 1), ('y', 1), ('time', 24)]:
                if not isinstance(data[key], (int, float)) or not math.isfinite(data[key]) or not 0 <= data[key] <= limit:
                    raise ValueError('Invalid frame position')
            if data['mode'] not in ('fine', 'terminal'):
                raise ValueError('Invalid rendering mode')
            variant = data.get('variant', 'original')
            if variant not in ('awakening', 'original', 'cortex', 'field', 'cortex3d', 'nebula', 'web', 'lightning'):
                raise ValueError('Invalid variant')
            settings = data.get('settings', {})
            if not isinstance(settings, dict):
                raise ValueError('Invalid settings')
            choices = {'view': ('network', 'logo'), 'word': ('p1', 'phaseone', 'alternate'), 'family': ('stepped', 'contour', 'synapse'), 'tribute': ('love', 'pie', 'portal', 'inspired', 'off')}
            for key, values in choices.items():
                if key in settings and settings[key] not in values:
                    raise ValueError('Invalid setting')
            for key in ('identity', 'easterEggs', 'morph', 'agents', 'threads', 'halo', 'pulse', 'gather', 'neurons', 'connections', 'signals', 'motion', 'heading', 'prompt'):
                if key in settings and not isinstance(settings[key], bool):
                    raise ValueError('Invalid toggle')
            for key, low, high in [('strength', .4, 2), ('speed', .25, 2), ('logoScale', .65, 1.4)]:
                if key in settings and (not isinstance(settings[key], (int, float)) or not math.isfinite(settings[key]) or not low <= settings[key] <= high):
                    raise ValueError('Invalid slider')
            allowed = set(choices) | {'identity', 'easterEggs', 'morph', 'agents', 'threads', 'halo', 'pulse', 'gather', 'neurons', 'connections', 'signals', 'motion', 'heading', 'prompt', 'strength', 'speed', 'logoScale'}
            if set(settings) - allowed or data.get('cycle', 0) not in (0, 1):
                raise ValueError('Unknown setting')
            prefix = 'data:image/png;base64,'
            if not data['screenshot'].startswith(prefix):
                raise ValueError('PNG required')
            png = base64.b64decode(data['screenshot'][len(prefix):], validate=True)
            if not png.startswith(b'\x89PNG\r\n\x1a\n'):
                raise ValueError('Invalid PNG')
        except (ValueError, KeyError, TypeError, AttributeError, binascii.Error):
            self.json_response(400, {'error': 'Invalid comment'})
            return
        ident = uuid.uuid4().hex[:12]
        row = {key: data[key] for key in ('x', 'y', 'time', 'mode', 'text')}
        row.update(settings=settings, cycle=data.get('cycle', 0), variant=variant, revision=4, id=ident, created=datetime.now(timezone.utc).isoformat(), screenshot=f'feedback/{ident}.png')
        FEEDBACK.mkdir(exist_ok=True)
        (ROOT / row['screenshot']).write_bytes(png)
        with (FEEDBACK / 'comments.jsonl').open('a') as stream:
            stream.write(json.dumps(row) + '\n')
        print('NEW FEEDBACK: ' + json.dumps(row), flush=True)
        self.json_response(201, row)


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--port', type=int, default=8765)
    args = parser.parse_args()
    server = HTTPServer(('127.0.0.1', args.port), Handler)
    print(f'Preview: http://127.0.0.1:{args.port}', flush=True)
    server.serve_forever()
