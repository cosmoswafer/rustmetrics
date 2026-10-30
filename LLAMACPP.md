# rustmetrics llamacpp — TUI monitor quick start

`rustmetrics llamacpp` connects to an **already running** `llama-server`
(llama.cpp) and charts its live activity in the terminal. It attaches
read-only, as-is: nothing to enable on the server, no restart, no model
loads triggered. It never starts or configures a server — point it at one
that is up.

## Quick start

```sh
cargo run --release -- llamacpp --url http://localhost:2464
```

That's it. If the server is reachable you get a live dashboard:

```
╭────────────────────────────────────────────────────────────────────────────╮
│ ◆ RUSTMETRICS / LLAMA.CPP                 ● LIVE  SESSION 0m06s            │
│ URL http://localhost:2464 · router         10 models, 1 loaded             │
╰────────────────────────────────────────────────────────────────────────────╯
╭ GENERATION ────────╮ ╭ PROMPT ───────────╮ ╭ KV CACHE ─────────╮ ╭ CONTEXT ─╮
│ 103.9 tok/s      ↗ │ │ 79.1 tok/s      → │ │ 304.1k tokens   → │ │ 0.10 % │
╰────────────────────╯ ╰───────────────────╯ ╰───────────────────╯ ╰──────────╯
╭ active model ─────╮ ╭ generation / bonsai27 ─────────────────────────────────╮
│ ● bonsai27 LOADED │ │ 200.0 ··············································· │
│ CTX 65.5k         │ │ 100.0 ▃▄▅▆▇███████████████████████████████████████████ │
╰───────────────────╯ ╰────────────────────────────────────────────────────────╯
 tab model   m metrics   r refresh   q quit        ● passive · prometheus
```

## Is my server running? What URL do I use?

The monitor needs the server's **base URL**. Check what's listening:

```sh
ss -tlnp | grep llama-server        # find the port
curl http://localhost:2464/health   # -> {"status":"ok"} means it's up
```

Common cases:

| Setup | URL |
| --- | --- |
| This machine's llama.cpp router (used by opencode here) | `http://localhost:2464` |
| Plain `llama-server` with defaults | `http://127.0.0.1:8080` |
| Custom port (`--port 9000`) | `http://127.0.0.1:9000` |
| Remote box | `http://<host>:<port>` (https OK) |

If you omit `--url`, the default is `http://127.0.0.1:8080` (llama.cpp's own
default), or set it once per shell with `export LLAMACPP_URL=http://localhost:2464`.

Both server shapes are supported and auto-detected:

- **router mode** — one port that manages many models, loading them on demand
  (what opencode talks to). Models are listed from `/v1/models`; slots and
  metrics are fetched per model with `?model=`.
- **instance mode** — a single `llama-server --model ...` process. Works the
  same; the single model is watched directly.

## What you see

- **live KPI cards** — current generation, prompt, KV cache and context values,
  with averages, peaks, trend arrows and miniature history plots.
- **active model panel** — the watched model's load state, context size, HF
  repository and runtime metadata. `tab` cycles only active/loaded models.
- **slot activity panel** — busy/idle slots, per-slot context meters, token
  counts and KV cache hits.
- **filled charts** — full-width generation and prompt throughput plus context
  histories, with automatic scaling, axes and sample counts.
- **Prometheus grid** — when `/metrics` is enabled, llama.cpp's own series are
  listed first in a compact multi-column panel.
- **request activity + footer** — real processing/deferred requests, decode
  counts, data source, history window and keyboard hints.

### Keys

| Key | Action |
| --- | --- |
| `tab` | Pin the next active/loaded model (charts follow it) |
| `m` | Toggle the Prometheus panel |
| `r` | Force a refresh now |
| `q` / `esc` / `ctrl-c` | Quit |

## Where the numbers come from

1. **Prometheus gauges** — `/metrics?model=...` supplies the server's
   instantaneous `llamacpp:predicted_tokens_seconds` and
   `llamacpp:prompt_tokens_seconds` values on every successful poll.
2. **Passive slot deltas** — between consecutive `/slots?model=...` snapshots,
   the monitor measures changes in `n_prompt_tokens_processed` and
   `next_token.n_decoded` for the same `id_task`. This keeps throughput charts
   moving when the metrics proxy fails, using real work from OpenCode or any
   other client. New tasks and counter resets establish a fresh baseline and
   never produce negative or cross-request rates. Slot and metrics requests run
   concurrently, and a busy slot endpoint cannot stall Prometheus updates.

The TUI is strictly read-only: it sends **no chat, completion, generation, or
model-loading requests**. It only polls `/health`, `/props`, `/v1/models`,
`/slots`, and `/metrics` for models that are already active. Sleeping and
unloaded router models are hidden from automatic selection and `tab` navigation.

## Useful recipes

```sh
# watch real OpenCode inference with half-second passive polling
rustmetrics llamacpp --url http://localhost:2464 --interval 0.5

# the normal TUI is always passive; no inference requests are sent
rustmetrics llamacpp --url http://localhost:2464

# follow a specific model from startup
rustmetrics llamacpp --url http://localhost:2464 --model gemma4

# keep 30 minutes of history, 4Hz-ish polling
rustmetrics llamacpp --url http://localhost:2464 --interval 0.5 --window 1800

# server behind an API key
rustmetrics llamacpp --url https://gpu-box:8080 --api-key sk-...

# no terminal: one passive poll, print summary, exit (script-friendly)
rustmetrics llamacpp --once --no-probe --url http://localhost:2464
```

`--once` exits non-zero if the server is down, so it works as a health probe
in scripts/cron.

## Troubleshooting

| Symptom | Meaning / fix |
| --- | --- |
| header shows `● down`, error `connection refused` | wrong `--url`/port, or the server is not running — verify with `curl <url>/health` |
| charts empty | no active model or no throughput observation yet; start a real request, press `tab` onto a loaded model, or pass `--model` |
| `metrics: HTTP 500: proxy error: Could not establish connection` | the router is alive but cannot reach the model child's `/metrics` endpoint; slot charts continue from `/slots`, and the error clears after a successful metrics poll |
| `metrics not supported (start llama-server with --metrics)` | Prometheus is unavailable; throughput uses passive `/slots` counter deltas instead |
| model row shows `◐ sleeping` | the router unloaded it from VRAM after idle; it wakes on the next real client request, never from this TUI |
