"""Paint an inert viewport and record raw input; never interpret survey keys."""
import os
from pathlib import Path
import socket
import struct
import sys
import tty

root = Path(os.environ['ATM_STANDIN_ROOT'])
rows = (root / 'frame.txt').read_bytes().split(b'\n')
assert len(rows) == 120
with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as bus:
    bus.connect(str(root / 'input.sock'))
    tty.setraw(0)
    out = sys.stdout.buffer
    out.write(b'\x1b[2J\x1b[?2004h')
    for i, row in enumerate(rows, 1):
        out.write(f'\x1b[{i};1H\x1b[0m'.encode() + row)
    out.write(b'\x1b[117;4H\x1b[6n')  # tmux replies after processing the paint
    out.flush()
    response = b''
    while not response.endswith(b'R'):
        part = os.read(0, 256)
        assert part
        response += part
    assert response == b'\x1b[117;4R', response
    bus.sendall(b'r')
    buffered = b''
    while data := os.read(0, 4096):
        buffered += data
        while b'\x1b[24~' in buffered:  # F12, an input-order barrier only
            keys, buffered = buffered.split(b'\x1b[24~', 1)
            bus.sendall(struct.pack('!I', len(keys)) + keys)
