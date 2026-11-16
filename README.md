# replayfloor

A one-binary probe for a single question about Helius LaserStream: how much
history can you actually replay before the stream tells you the slot is gone?

## Result: one hour on devnet, 16 November 2026

Sixty one samples over sixty minutes against `laserstream-devnet-ewr`, no
failed polls. Raw JSONL kept locally, not in this repository.

| | |
|---|---|
| samples | 61 total, 61 usable, 0 failed |
| span | 60.0 min |
| head rate | 4.18 slots/s observed |
| documented retention | 3000 slots, about 20 min |
| observed retention | 864022 / 864167 / 864301 slots (min / median / max) |
| same, in minutes | 3446.7 / 3447.2 / 3447.8 min |
| drift over the run | +14 slots, +14 slots/h |
| window shape | fixed window |
| first available | 522160450 to 522175477 |
| head | 523024685 to 523039726 |
| get_slot latency | 213 / 230 / 343 ms (min / median / max) |
| replay_info latency | 215 / 229 / 368 ms (min / median / max) |

The replay buffer holds 864,167 slots at the median, 288 times the documented
3000. At the 4.18 slots per second this endpoint actually produced, that is
3447 minutes, about 57 hours of chain history. The documented 20 minutes comes
from applying 2.5 slots per second to 3000 slots, so the wrong part is the slot
count, not the arithmetic.

The depth moved by 14 slots across the hour while the head advanced 15,041, and
`firstAvailable` advanced 15,027. That is a fixed window: the floor stays a
constant distance behind the head instead of a constant age. A time based
buffer would have looked the same over this span, so the shape line alone does
not settle it; the depth being 288 times the documented figure does.

## The open question

The LaserStream documentation says the replay buffer holds 3000 slots, about
20 minutes of chain history. While writing the gap recovery example in
[helius-starter-kit](https://github.com/maxxexee/helius-starter-kit) I noticed
`firstAvailable` sitting roughly 864,000 slots behind the head, which is closer
to 48 hours than to 20 minutes at 2.5 slots per second. Two numbers cannot both
be right.

That example stopped at the observation. This probe measures it instead: it
polls the live head and `firstAvailable` on an interval for an hour, writes raw
samples, and turns them into one table.

## What it measures

Two gRPC calls per sample, both against the same connection:

- `get_slot(Processed)` for the live head.
- `subscribe_replay_info()` for `firstAvailable`, the oldest slot the endpoint
  will still serve.

The difference is the replay depth in slots. The depth alone does not tell you
whether retention is a fixed slot count or a timer, so the report also tracks
how the depth drifts over the run and converts it to minutes using the head rate
that was *observed*, not the nominal 2.5 slots per second.

Samples are written as JSONL with no derived values baked in:

```json
{"ts_ms":1794860088474,"head":523024685,"first_available":522160450,"delta_slots":864235,"head_ms":219,"replay_ms":215,"error":null}
```

A failed poll is recorded as a sample with an `error` string, never as an exit.
An hour long run has to survive timeouts, and a dead window is data too.

## Run it

```bash
cp .env.example .env    # set HELIUS_API_KEY
cargo run --release -- probe --hours 1 --out devnet-1h.jsonl
cargo run --release -- report devnet-1h.jsonl
```

`--endpoint` takes a full URL or a bare region name such as `mainnet-ewr`.
`--interval-secs` defaults to 60. `report` accepts any file the probe wrote,
including one that was interrupted.

## How to read the result

Three outcomes are possible, and all three are worth publishing:

- Depth near 3000 slots: the documentation is right and my observation was
  wrong, probably a misread of `firstAvailable` during a restart.
- Depth near 3000 slots but drift over time: the window is time based and the
  documented slot count is a rounding of it.
- Depth far above 3000 slots: the documented figure describes something other
  than the actual replay buffer.

The third case is what the devnet endpoint does: 864,167 slots against a
documented 3000. The limits below apply before generalising that to mainnet.

## Limits

- Devnet only. LaserStream mainnet on a free plan refuses the connection, so
  every number here comes from `laserstream-devnet-ewr`. Devnet retention is
  not guaranteed to match mainnet.
- One region, one hour. A single hour shows the depth is stable, not that it
  stays stable across a validator restart or a purge cycle longer than the run.
- `firstAvailable` is reported by the server. If it lies, or if it only reflects
  the last purge rather than an authoritative floor, this probe repeats the lie
  rather than detecting it. Confirming it would mean asking for the oldest slot
  by number and seeing whether the stream serves it.
- Retention may be tier dependent. A Business or Professional key could see a
  different buffer than a free one.

## License

MIT
