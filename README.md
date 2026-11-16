# replayfloor

A one-binary tool for a single question about Helius LaserStream: how much
history can you actually replay before the stream tells you the slot is gone?

Measured on devnet: **864,149 slots**, and the floor is real, not just
reported. That is 288 times the documented 3000.

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

## Verification: the floor is real, not just reported

The number above comes from the server's own `firstAvailable`. A server can
claim a floor it will not honour, so `floor` asks for old slots directly and
watches where the stream actually starts. It grows the request 1.5x while slots
serve, then bisects until the served and refused ages are 50 slots apart.

| | |
|---|---|
| endpoint | https://laserstream-devnet-ewr.helius-rpc.com |
| head at start | 523058130 |
| claimed floor | 522193975 |
| claimed depth | 864155 slots |
| verified depth | 864149 slots (largest age served exactly) |
| first failing age | 864184 slots (refused or silent) |
| resolution | 50 slots (bisection tolerance) |
| verdict | claim holds within the 50 slot resolution of the search |
| attempts | 25 (0 silent, 0 clamped) |
| attempt latency | 234 / 459 / 3806 ms (min / median / max) |

Two details make that a stronger result than "data arrived":

- Every served attempt returned an update at exactly the slot requested, not a
  later one. A clamped stream, where the server quietly starts at its floor
  regardless of what you asked for, would show as `Clamped` here and does not.
- The refusal at 864,184 slots was an explicit gRPC `OutOfRange`, so the
  endpoint tells you when you have asked for a slot it has purged. It is not a
  silent drop.

The first run of this command reported 863,833 slots as the floor with a
verdict that the claim was optimistic. That attempt had failed with
`Some resource has been exhausted: h2 protocol error`, a transport blip that
the search had recorded as the boundary. Retries on a fresh connection, and an
abort when transport keeps failing, are why the second run is trustworthy. A
network error must never be allowed to look like the end of the replay window.

## The open question

The LaserStream documentation says the replay buffer holds 3000 slots, about
20 minutes of chain history. While writing the gap recovery example in
[helius-starter-kit](https://github.com/maxxexee/helius-starter-kit) I noticed
`firstAvailable` sitting roughly 864,000 slots behind the head, which is closer
to 48 hours than to 20 minutes at 2.5 slots per second. Two numbers cannot both
be right.

That example stopped at the observation. This tool measures it instead:
`probe` polls the live head and `firstAvailable` on an interval, `floor` asks
for old slots directly to check the reported floor, and `report` turns the
samples into one table.

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

# what does the endpoint claim, over time?
cargo run --release -- probe --hours 1 --out devnet-1h.jsonl
cargo run --release -- report devnet-1h.jsonl

# does the claim hold, right now?
cargo run --release -- floor --tolerance 50 --out floor.jsonl
```

`--endpoint` takes a full URL or a bare region name such as `mainnet-ewr`.
`--interval-secs` defaults to 60. `--tolerance` is how close the bisection gets
before it stops; 50 slots is about 12 seconds of devnet chain time. `report`
accepts any file the probe wrote, including one that was interrupted.

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
- `floor` checks the reported floor against what the stream serves, but only
  for slots. It does not prove that transactions, blocks or account writes
  survive to the same depth; those filters can have their own limits.
- `floor` treats five seconds of silence at a given age as a refusal. A stream
  that was merely slow would look the same, which is why every attempt records
  its latency and why the retry exists.
- Retention may be tier dependent. A Business or Professional key could see a
  different buffer than a free one.

## License

MIT
