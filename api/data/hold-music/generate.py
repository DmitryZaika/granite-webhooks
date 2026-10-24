"""Synthesizes Granite Phone's built-in hold music (original, CC0).

python3 holdmusic.py OUT_DIR  ->  calm.wav, bright.wav, ambient.wav (22.05 kHz mono)
Each track is a seamless loop: its notes decay before the end.
"""
import math, random, struct, sys, wave

RATE = 22050

def note_hz(n):  # MIDI note
    return 440.0 * 2 ** ((n - 69) / 12)

def render(events, seconds, voice):
    buf = [0.0] * int(seconds * RATE)
    for start, midi, dur, vel in events:
        f = note_hz(midi)
        s0 = int(start * RATE)
        n = int(dur * RATE)
        for i in range(n):
            j = s0 + i
            if j >= len(buf):
                break
            t = i / RATE
            buf[j] += vel * voice(f, t, dur)
    return buf

def piano(f, t, dur):
    env = math.exp(-3.0 * t) * min(1.0, t * 200)
    rel = min(1.0, (dur - t) * 20)
    w = 2 * math.pi * f * t
    return env * rel * (math.sin(w) + 0.35 * math.sin(2 * w) + 0.12 * math.sin(3 * w) + 0.05 * math.sin(4 * w))

def marimba(f, t, dur):
    env = math.exp(-6.0 * t) * min(1.0, t * 400)
    rel = min(1.0, (dur - t) * 30)
    w = 2 * math.pi * f * t
    return env * rel * (math.sin(w) + 0.25 * math.sin(4 * w) * math.exp(-20 * t))

def pad(f, t, dur):
    att = min(1.0, t / 1.5)
    rel = min(1.0, (dur - t) / 1.5)
    w = 2 * math.pi * f * t
    return att * rel * (math.sin(w) + 0.5 * math.sin(w * 1.003) + 0.3 * math.sin(2 * w * 0.998)) * 0.5

CHORDS = {  # root-position triads + bass, MIDI
    'C': [60, 64, 67], 'Am': [57, 60, 64], 'F': [53, 57, 60], 'G': [55, 59, 62],
    'Em': [52, 55, 59], 'Dm': [50, 53, 57],
}

def calm():
    beat = 60 / 72
    prog = ['C', 'Am', 'F', 'G', 'C', 'Em', 'F', 'G'] * 2
    ev = []
    t = 0.0
    for ch in prog:
        notes = CHORDS[ch]
        ev.append((t, notes[0] - 12, beat * 4, 0.35))
        pattern = [notes[0], notes[1], notes[2], notes[1] + 12, notes[2], notes[1], notes[2] + 12, notes[1]]
        for k, m in enumerate(pattern):
            ev.append((t + k * beat / 2, m, beat * 1.8, 0.22))
        t += beat * 4
    return render(ev, t + 0.0, piano), t

def bright():
    beat = 60 / 100
    prog = ['F', 'G', 'Em', 'Am', 'Dm', 'G', 'C', 'C'] * 2
    random.seed(7)
    ev = []
    t = 0.0
    for ch in prog:
        notes = CHORDS[ch]
        ev.append((t, notes[0] - 12, beat * 2, 0.4))
        ev.append((t + beat * 2, notes[0] - 12, beat * 2, 0.35))
        for k in range(8):
            m = notes[k % 3] + (12 if k in (3, 6) else 0)
            ev.append((t + k * beat / 2, m, beat * 0.9, 0.25 if k % 2 == 0 else 0.18))
        t += beat * 4
    return render(ev, t, marimba), t

def ambient():
    bar = 6.0
    prog = ['C', 'Am', 'F', 'G', 'Em', 'F', 'Dm', 'G']
    ev = []
    t = 0.0
    for ch in prog:
        for m in CHORDS[ch]:
            ev.append((t, m, bar, 0.16))
        ev.append((t, CHORDS[ch][0] - 12, bar, 0.2))
        t += bar
    return render(ev, t, pad), t

def write(path, buf):
    peak = max(1e-9, max(abs(x) for x in buf))
    gain = 0.8 / peak
    with wave.open(path, 'wb') as w:
        w.setnchannels(1); w.setsampwidth(2); w.setframerate(RATE)
        w.writeframes(b''.join(struct.pack('<h', int(x * gain * 32767)) for x in buf))

out = sys.argv[1]
for name, fn in [('calm', calm), ('bright', bright), ('ambient', ambient)]:
    buf, secs = fn()
    write(f'{out}/{name}.wav', buf)
    print(name, round(secs, 1), 's')
