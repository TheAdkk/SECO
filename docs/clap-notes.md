# CLAP reference notes — Phase 0 reconnaissance

Everything below was read directly from the CLAP headers, not recalled from memory.

- Source: `reference/clap` — clone of <https://github.com/free-audio/clap>
- Pinned commit: `195b42a004144fab0b3cf95e9c067187d15365b7` (2026-07-13, tag `1.2.10`)
- CLAP version constants: `1.2.10` (`version.h:23-25`)

All `file:line` citations refer to `reference/clap/include/clap/` at that commit.

---

## 1. Transport: how the host exposes musical position and tempo

This is the heart of `zape`, so it gets the most detail.

### 1.1 Two delivery paths

The doc comment on `clap_event_transport` (`events.h:274-279`) states there are
exactly two ways the host communicates transport:

1. **Per block:** `clap_process.transport` points to a `clap_event_transport_t`
   describing the state **at sample 0** of the block (`process.h:43-45`).
2. **Sample-accurate (optional):** the host *may* additionally push
   `CLAP_EVENT_TRANSPORT` events (`type = 9`, `events.h:118`) into
   `clap_process.in_events`, each carrying a full `clap_event_transport_t` whose
   `header.time` is the sample offset within the block (`events.h:20`).

Critical null case (`process.h:43-44`):

> ```
> // time info at sample 0
> // If null, then this is a free running host, no transport events will be provided
> ```

So `transport == NULL` is legal and means: no tempo, no position, ever. This is
the "host without timeline" edge case — `zape` must free-run on an internal
phase accumulator at an assumed tempo.

### 1.2 Fixed-point time types (`fixedpoint.h`)

```c
typedef int64_t clap_beattime;   // fixedpoint.h:15
typedef int64_t clap_sectime;    // fixedpoint.h:16

static const int64_t CLAP_BEATTIME_FACTOR = 1LL << 31;  // fixedpoint.h:12
static const int64_t CLAP_SECTIME_FACTOR  = 1LL << 31;  // fixedpoint.h:13
```

Usage comment (`fixedpoint.h:6-9`):

> ```
> /// We use fixed point representation of beat time and seconds time
> /// Usage:
> ///   double x = ...; // in beats
> ///   clap_beattime y = round(CLAP_BEATTIME_FACTOR * x);
> ```

So both are signed 64-bit **Q32.31** fixed point. Decode in Rust:
`beats = song_pos_beats as f64 / (1u64 << 31) as f64`. The factor is documented
as "This will never change" (`fixedpoint.h:11`).

Note: `f64` has a 52-bit mantissa; converting the full 63-bit range loses
precision beyond ~2^21 beats (~1 week at 120 BPM). Irrelevant for us, but worth
knowing: for phase computation we can take `fract()` in fixed point first
(`pos & (FACTOR*cycle - 1)`-style, or `i64 % (FACTOR * cycle_beats)`) and only
then convert, if we ever care. v1 converts to `f64` directly.

### 1.3 `clap_event_transport` fields (`events.h:280-302`)

```c
typedef struct clap_event_transport {
   clap_event_header_t header;

   uint32_t flags; // see clap_transport_flags

   clap_beattime song_pos_beats;   // position in beats
   clap_sectime  song_pos_seconds; // position in seconds

   double tempo;     // in bpm
   double tempo_inc; // tempo increment for each sample and until the next
                     // time info event

   clap_beattime loop_start_beats;
   clap_beattime loop_end_beats;
   clap_sectime  loop_start_seconds;
   clap_sectime  loop_end_seconds;

   clap_beattime bar_start;  // start pos of the current bar
   int32_t       bar_number; // bar at song pos 0 has the number 0

   uint16_t tsig_num;   // time signature numerator
   uint16_t tsig_denom; // time signature denominator
} clap_event_transport_t;
```

Field-by-field, with type, unit, and the flag that gates validity:

| Field | Type | Unit | Valid when |
|---|---|---|---|
| `song_pos_beats` | `clap_beattime` (i64 Q32.31) | beats | `HAS_BEATS_TIMELINE` |
| `song_pos_seconds` | `clap_sectime` (i64 Q32.31) | seconds | `HAS_SECONDS_TIMELINE` |
| `tempo` | `double` | BPM | `HAS_TEMPO` |
| `tempo_inc` | `double` | BPM per sample, applies until next transport event | `HAS_TEMPO` |
| `loop_start_beats`, `loop_end_beats` | `clap_beattime` | beats | `IS_LOOP_ACTIVE` + `HAS_BEATS_TIMELINE` |
| `loop_start_seconds`, `loop_end_seconds` | `clap_sectime` | seconds | `IS_LOOP_ACTIVE` + `HAS_SECONDS_TIMELINE` |
| `bar_start` | `clap_beattime` | beats (position where current bar starts) | `HAS_BEATS_TIMELINE` |
| `bar_number` | `int32_t` | bar index, bar at song pos 0 is 0 | `HAS_BEATS_TIMELINE` |
| `tsig_num`, `tsig_denom` | `uint16_t` | time signature | `HAS_TIME_SIGNATURE` |

Caveat on the "Valid when" column: the header only *names* the flags
(`HAS_TEMPO`, `HAS_BEATS_TIMELINE`, `HAS_SECONDS_TIMELINE`,
`HAS_TIME_SIGNATURE`); it does not spell out a field→flag table. The mapping
above is the only sensible reading (each `HAS_*` flag gates the fields in its
domain), but the loop/bar rows are interpretation, not quoted spec. The four
rows that matter for `zape` (`song_pos_beats`, `tempo`) are unambiguous.

### 1.4 Transport flags (`events.h:263-272`)

```c
enum clap_transport_flags {
   CLAP_TRANSPORT_HAS_TEMPO            = 1 << 0,
   CLAP_TRANSPORT_HAS_BEATS_TIMELINE   = 1 << 1,
   CLAP_TRANSPORT_HAS_SECONDS_TIMELINE = 1 << 2,
   CLAP_TRANSPORT_HAS_TIME_SIGNATURE   = 1 << 3,
   CLAP_TRANSPORT_IS_PLAYING           = 1 << 4,
   CLAP_TRANSPORT_IS_RECORDING         = 1 << 5,
   CLAP_TRANSPORT_IS_LOOP_ACTIVE       = 1 << 6,
   CLAP_TRANSPORT_IS_WITHIN_PRE_ROLL   = 1 << 7,
};
```

These are the *transport's own* `flags` field. Do not confuse with
`header.flags`, which holds `clap_event_flags` (`IS_LIVE`, `DONT_RECORD`,
`events.h:29-39`) — a different namespace on the same struct.

### 1.5 What is "a beat"?

The header says only "position in beats" / tempo "in bpm". It does **not**
define whether a beat is a quarter note or the time-signature denominator unit.

**Empirically resolved (Phase 2, 2026-07): a beat is a quarter note.**
Observed via zape's transport trace log:

- REAPER 7.77 (macOS), 4/4 at 120 BPM: `song_pos_beats` advances at exactly
  2.000/s (= 120/60), matching `song_pos_seconds` sample-for-sample
  (e.g. `ppq=40.080 sec=20.040` … `ppq=41.104 sec=20.552`), and `bar_start`
  increments by +4.0 per 4/4 bar (4 quarter notes).
- clap-validator's synthetic transport at 110 BPM: `ppq/sec = 1.8333 =
  110/60`, consistent.

A 6/8 discriminator run was unnecessary — the 4/4 rate + bar length already
pin the unit. Caveat: verified in REAPER (plus clap-validator); a second real
DAW (Bitwig) has not cross-checked this yet.

### 1.6 Implications for `zape`'s edge cases

- **No transport at all:** `process->transport == NULL` → free-run: internal
  phase accumulator, assume 120 BPM.
- **Transport but no beats timeline:** `HAS_BEATS_TIMELINE` clear → accumulate
  phase from `tempo` (if `HAS_TEMPO`) else 120 BPM.
- **Stopped transport:** `IS_PLAYING` clear → hosts may keep `song_pos_beats`
  frozen; keep ducking via the internal accumulator (free-run behavior).
- **Loop/seek jumps:** detect discontinuity by comparing the block's
  `song_pos_beats` against our predicted position; resync phase hard, short
  fade if the gain step exceeds threshold. `IS_LOOP_ACTIVE` + loop points can
  make jumps *predictable*, but v1 only needs detection, not prediction.
- **Mid-block tempo change:** delivered as `CLAP_EVENT_TRANSPORT` in
  `in_events` at `header.time > 0`, and/or as `tempo_inc` ramps. v1 reads
  transport once per block (documented limitation); we should still *consume*
  transport events from the list to use the latest state, we just won't split
  the block on them.
- **`steady_time`** (`process.h:31-38`): steady sample counter, `-1` if
  unavailable, otherwise ≥0 and increases by ≥ `frames_count` per call; may
  jump backward after `reset()` (`plugin.h:87`). Useful as a fallback clock,
  but the internal accumulator doesn't strictly need it.

---

## 2. Entry point (`entry.h`)

- The DSO exports exactly one symbol: `const clap_plugin_entry_t clap_entry`
  (`entry.h:132`), a struct of three function pointers plus `clap_version`:
  - `bool init(const char *plugin_path)` — `plugin_path` is the DSO path
    (Linux/Windows) or bundle path (macOS) (`entry.h:93`).
  - `void deinit(void)`
  - `const void *get_factory(const char *factory_id)` — `[thread-safe]`,
    returns NULL for unknown factories (`entry.h:120-128`).
- **Multiple init/deinit calls can happen as of CLAP 1.2.0** (`entry.h:34-60`):
  a host wrapping a CLAP inside a CLAP can cause nested `init()` calls. The
  spec requires the counter + mutex defense only "if undertaking non trivial
  non idempotent actions" (`entry.h:34-36`). SECO's `init`/`deinit` do nothing
  (trivially idempotent), so they need no counter and no mutex — just a
  comment stating why. `init` must be fast (hosts scan), no GUI, no user
  interaction (`entry.h:78-82`).
- `init` may be called from any thread, but never concurrently with any other
  symbol of the DSO (`entry.h:95-99`).
- **Plugin search paths** (`entry.h:12-32`) — for the Phase 1 install
  instructions:
  - Linux: `~/.clap`, `/usr/lib/clap`
  - Windows: `%COMMONPROGRAMFILES%\CLAP`, `%LOCALAPPDATA%\Programs\Common\CLAP`
  - macOS: `/Library/Audio/Plug-Ins/CLAP`, `~/Library/Audio/Plug-Ins/CLAP`
  - Plus `CLAP_PATH` env var (`:`-separated Unix, `;`-separated Windows).
  - Directories are searched recursively for files/bundles ending in `.clap`.

## 3. Plugin factory (`factory/plugin-factory.h`)

Note: this header moved; it lives in `factory/`, not the repo root as older
docs suggest.

- Factory ID string: `CLAP_PLUGIN_FACTORY_ID = "clap.plugin-factory"` (`:7`).
- `clap_plugin_factory_t` (`:18-39`), all `[thread-safe]`:
  - `uint32_t get_plugin_count(factory)`
  - `const clap_plugin_descriptor_t *get_plugin_descriptor(factory, index)` —
    descriptor valid until `deinit()`.
  - `const clap_plugin_t *create_plugin(factory, host, plugin_id)` — the
    `clap_host` pointer stays valid until after `plugin->destroy()`. **The
    plugin must not call host callbacks inside `create_plugin`** (`:33`);
    host access belongs in `clap_plugin.init` (`plugin.h:49-51`).

## 4. Plugin descriptor and lifecycle (`plugin.h`)

- `clap_plugin_descriptor_t` (`plugin.h:12-39`): `clap_version`, then strings
  `id` (reverse-URI, mandatory), `name` (mandatory), `vendor`, `url`,
  `manual_url`, `support_url`, `version`, `description`, and a
  NULL-terminated `const char *const *features` array. Standard feature
  strings in `plugin-features.h` — for `zape`: `"audio-effect"`,
  `"stereo"` (`plugin-features.h:19,77`).
- `clap_plugin_t` (`plugin.h:41-110`): `desc`, `plugin_data` (our instance
  pointer), then:

  | fn | thread | notes |
  |---|---|---|
  | `init` | `[main-thread]` | false → host destroys instance; host ext setup goes here |
  | `destroy` | `[main-thread & !active]` | must deactivate first |
  | `activate(sample_rate, min_frames, max_frames)` | `[main-thread & !active]` | allocation happens HERE; frame counts within `[1, INT32_MAX]`; latency/ports frozen until deactivate |
  | `deactivate` | `[main-thread & active]` | |
  | `start_processing` | `[audio-thread & active & !processing]` | |
  | `stop_processing` | `[audio-thread & active & processing]` | |
  | `reset` | `[audio-thread & active]` | clear buffers/state; param values unchanged; `steady_time` may jump back |
  | `process` | `[audio-thread & active & processing]` | all `clap_process_t` pointers valid only during the call |
  | `get_extension` | `[thread-safe]` | returned pointer valid until `destroy`; callable from `init` onward, never before |
  | `on_main_thread` | `[main-thread]` | response to `host->request_callback` |

- `activate` giving `max_frames_count` is where SECO pre-allocates anything it
  needs, keeping `process()` allocation-free by construction.

## 5. `process()` contract (`process.h`)

- Return type `clap_process_status` (`int32_t`), values (`process.h:10-27`):
  `CLAP_PROCESS_ERROR = 0`, `CONTINUE = 1`, `CONTINUE_IF_NOT_QUIET = 2`,
  `TAIL = 3`, `SLEEP = 4`. `zape` v1 returns `CONTINUE` (it must keep
  running even on silence — the duck curve is time-driven, and bypass must not
  stop processing per `params.h:153-155`).
- `clap_process_t` fields (`process.h:30-62`): `steady_time` (§1.6),
  `frames_count`, `transport` (§1.1), `audio_inputs` / `audio_outputs` +
  counts (buffer count must match the audio-ports extension; index maps to
  port index), `in_events`, `out_events`.
- `in_events` is read-only and **sorted by sample time** (`process.h:56-57`,
  `events.h:344`). Anything we push to `out_events` must also be sorted
  (`events.h:355`); `try_push` copies the event (`events.h:359-362`).
- The header does not promise `frames_count > 0` (activate's `[min,max]`
  bound suggests ≥1, but hosts misbehave); a `frames_count == 0` call — e.g.
  a param-flush-shaped process call — must be a graceful no-op. Defensive
  handling stays.

## 6. Audio buffers and ports

### `audio-buffer.h`

- `clap_audio_buffer_t` (`:26-33`): `float **data32`, `double **data64`
  (exactly one set), `channel_count`, `latency`, `constant_mask`.
- `constant_mask` bit N set ⇒ channel N is constant; **the buffer is still
  fully filled with that value** (`:18-25`), so ignoring the mask is safe
  (it's an optimization hint, not a validity condition). v1 ignores it.
- "Buffers nulos": with 32-bit processing, `data64` will be NULL; a paranoid
  adapter also checks `data32`/`data32[ch]` before building slices.

### `ext/audio-ports.h`

- Extension ID: `CLAP_EXT_AUDIO_PORTS = "clap.audio-ports"` (`:16`). Without
  it the plugin has no audio ports (`:10`).
- **32-bit support is mandatory for plugins; 64-bit optional** (`:12`). SECO
  v1: 32-bit only.
- `clap_plugin_audio_ports_t` (`:68-80`): `count(plugin, is_input)` and
  `get(plugin, index, is_input, *info)`, both `[main-thread]`.
- `clap_audio_port_info_t` (`:42-65`): stable `id`, `name[CLAP_NAME_SIZE]`,
  `flags`, `channel_count`, `port_type` (compare against
  `CLAP_PORT_STEREO = "stereo"`, `:18`), `in_place_pair`.
- `zape`: 1 in + 1 out, stereo, `CLAP_AUDIO_PORT_IS_MAIN` (`:28` — main
  port must be index 0), `in_place_pair` set to the paired id (we process
  in-place safely) — or `CLAP_INVALID_ID` (`id.h:8` = `UINT32_MAX`) to start.
- Port config may only change while deactivated (`:14`, `:67`).

## 7. Events and the params extension

### Event basics (`events.h`)

- Every event starts with `clap_event_header_t` (`:18-24`): `size` (bytes,
  including header), `time` (sample offset in block), `space_id`, `type`,
  `flags`. Core events use `CLAP_CORE_EVENT_SPACE_ID = 0` (`:27`). **Always
  check `space_id` before interpreting `type`.**
- Events are contiguous, memcpy-able blobs (`:14-17`).
- Types we care about: `CLAP_EVENT_PARAM_VALUE = 5` (`clap_event_param_value_t`,
  `:222-237`: `param_id`, `cookie`, note-targeting fields all `-1` for global,
  `double value`) and `CLAP_EVENT_TRANSPORT = 9` (§1). We can ignore note
  events, `PARAM_MOD`, gestures, and MIDI for v1.

### `ext/params.h`

- Extension ID: `CLAP_EXT_PARAMS = "clap.params"` (`:127`).
- Plugin side (`:258-307`), all `[main-thread]` except flush: `count`,
  `get_info`, `get_value`, `value_to_text`, `text_to_value`, and
  `flush(plugin, in, out)` which is `[active ? audio-thread : main-thread]`
  (`:303`) — the host uses it to deliver param changes while not processing.
  `get_value` on the main thread while audio runs ⇒ parameter storage must be
  atomics, not plain fields.
- `clap_param_info_t` (`:211-256`): stable `clap_id id`, `flags`, `cookie`
  (optional fast-path pointer; host may echo it or pass NULL — must handle
  NULL, `:234-241`), `name[CLAP_NAME_SIZE]`, `module[CLAP_PATH_SIZE]`,
  `min_value` / `max_value` / `default_value` (plain values, finite).
- Flags for `zape`'s four params (`:133-207`):
  - `rate`: `IS_STEPPED | IS_ENUM | IS_AUTOMATABLE` (`IS_ENUM` requires
    `IS_STEPPED`, `:203-206`; every value needs non-blank `value_to_text`).
  - `mix`: `IS_AUTOMATABLE`.
  - `curve`: `IS_STEPPED | IS_ENUM | IS_AUTOMATABLE`.
  - `bypass`: `IS_STEPPED | IS_BYPASS | IS_AUTOMATABLE` (`IS_BYPASS` merges
    with the host bypass button, implies stepped, min 0 / max 1; **bypass must
    not stop the host calling `process()`** — implement as passthrough inside
    `process`, `:149-156`).
- Value model: host sends plain-value `double` automation; "the value heard is
  param_value + param_mod" (`:100-107`) — no `PARAM_MOD` support in v1 means
  we simply don't set `IS_MODULATABLE`.
- **Persistence rule** (`:101-107`): hosts should NOT save parameter values
  for plugins lacking the state extension. So `zape` needs `ext/state` no
  later than Phase 3, or projects won't recall settings.

## 8. State extension (`ext/state.h`, `stream.h`)

- ID: `CLAP_EXT_STATE = "clap.state"` (`:18`). `save(plugin, ostream)` /
  `load(plugin, istream)`, both `[main-thread]`.
- Streams may read/write partially; **must loop** until done; `read` returns 0
  at EOF, -1 on error; `write` returns -1 on error (`stream.h:10-16,22-34`).
- Host side has `mark_dirty`; param changes imply dirty automatically
  (`state.h:36-41`).

## 9. Threading model (`ext/thread-check.h` + tags)

- Two symbolic threads: `main-thread` (GUI/lifecycle; one OS thread for the
  plugin's lifetime) and `audio-thread` (`:11-51`).
- **The audio-thread is symbolic**: a host with a thread pool may run
  successive `process()` calls on *different OS threads*; the guarantee is
  only that one instance is never on two audio threads at once (`:30-40`).
  Consequences for SECO:
  - `[audio-thread]` functions need no internal locking against each other.
  - The debug allocation-detector flag must be *scoped per `process()` call*
    (set on entry, cleared on exit, thread-local), never "latched once for the
    audio thread" — there is no single audio thread.
  - `RtContext` being `!Send`/`!Sync` and per-call matches this model exactly.
- `[thread-safe]` functions may be called from any thread, concurrently
  (`:64-66`).
- Host may implement `clap_host_thread_check` (`is_main_thread` /
  `is_audio_thread`) — useful for debug assertions later.

## 10. FFI/ABI notes for the Rust side (`private/macros.h`)

- `CLAP_ABI` = `__cdecl` on Windows, empty elsewhere (`macros.h:20-26`).
  Rust's `extern "C"` is cdecl on x86 and the standard C ABI everywhere else ⇒
  **all function pointers are plain `extern "C" fn`**. No `extern "system"`
  anywhere.
- `CLAP_EXPORT` = `dllexport` / `visibility("default")` (`macros.h:3-18`). In
  Rust: `#[no_mangle] pub static clap_entry: ClapPluginEntry` in a
  `cdylib` crate. The symbol name must be exactly `clap_entry`.
- All structs are plain C structs ⇒ `#[repr(C)]` mirrors, field order exactly
  as in the headers. Bools are C `bool` (1 byte) via `private/std.h`'s
  `stdbool.h` ⇒ Rust `bool` is ABI-compatible per the Rust reference.
- Strings are NUL-terminated `*const c_char`, static lifetime for descriptor
  fields (valid until `deinit`).
- `clap_id` = `uint32_t`, `CLAP_INVALID_ID = UINT32_MAX` (`id.h:6-8`).
- `CLAP_NAME_SIZE = 256`, `CLAP_PATH_SIZE = 1024` (`string-sizes.h:8-16`) —
  fixed-size char arrays inside `clap_param_info` / `clap_audio_port_info`.
- `clap_version_is_compatible(v)` ⇔ `v.major >= 1` (`version.h:38-42`).

## 11. Phase 2 empirical results (REAPER 7.77, macOS; log: zape trace)

1. **Beat unit: quarter note.** Answered — see §1.5.
2. **Stopped transport (REAPER):** `IS_PLAYING` clears but every `HAS_*`
   flag stays set and `song_pos_beats` **freezes** at the stop position
   (observed `flags 0b00011111 → 0b00001111`, ppq pinned at 108.0). The
   free-run internal accumulator is therefore mandatory for `zape`, not a
   nice-to-have.
3. **Mid-block transport events:** none observed — `tp_events=0` across
   every REAPER and clap-validator run. `tempo_inc` behavior under tempo
   ramps remains unobserved (constant-tempo sessions only). Per-block
   `clap_process.transport` was the only delivery seen.
4. **Playhead drag while stopped:** not yet observed (not exercised).
5. **`transport == NULL`:** never seen — `has_tp=true` on every block in
   both hosts tested.
6. **Loop wrap (REAPER):** `song_pos_beats` jumps **backward with no
   interpolation** at the loop point (observed `blocks=3847 ppq=121.27 →
   blocks=3883 ppq=113.66`). Unhandled, this clicks on every pass — this is
   the transport-jump edge case Phase 3 must absorb (hard resync + declick).

Pending: Bitwig cross-check of all of the above (F2 closed on REAPER data);
tempo-ramp behavior of `tempo_inc`.
