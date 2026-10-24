# Built-in hold music

What callers hear while a Granite Phone user has them on hold, unless the
user uploaded their own track (`api/src/phone/hold_music.rs`). The API
serves them at `GET /v1/public/hold-music/{track}.mp3`; Telnyx plays them
from there in a loop.

| File | Name in the app | Length |
| --- | --- | --- |
| `calm.mp3` | Calm piano | 53 s |
| `bright.mp3` | Bright marimba | 38 s |
| `ambient.mp3` | Soft ambient | 48 s |

## License

Original music, synthesized for Granite Manager by `generate.py` in this
folder (pure Python note synthesis: no samples, recordings or third-party
melodies; the chord progressions are common I-vi-IV-V style progressions,
which are not copyrightable). Released under CC0 1.0 (public domain
dedication), so the company may use, change and redistribute them freely.

## Regenerating

```
python3 generate.py /tmp/hm
for n in calm bright ambient; do
  ffmpeg -y -i /tmp/hm/$n.wav -ac 1 -ar 22050 -b:a 48k $n.mp3
done
```

Each track stays under 400 KB (22.05 kHz mono, 48 kbps MP3), so the API
binary carries them.
