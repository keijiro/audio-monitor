# Development

## Build and checks

Use stable Rust and Xcode Command Line Tools on macOS. Dependencies are managed with Cargo; keep `Cargo.lock` with the application to preserve resolved versions.

```sh
cargo build --release
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
```

Once dependencies have been fetched, add `--offline` to build, test, and Clippy commands when working without network access.

## Source layout

- `src/main.rs`: terminal lifecycle, device selection, feedback confirmation, and rendering.
- `src/meters.rs`: responsive input meter rendering and UI-thread peak/clip holds.
- `src/audio.rs`: device configuration, stream lifecycle, metering, buffering, and sample-rate conversion.
- `src/safety.rs`: device-name feedback heuristic and its tests.
- `src/state.rs`: JSON persistence, buffer validation, and unambiguous device-name matching.

## Audio architecture

CPAL uses Core Audio on macOS. Input and output configurations are selected independently: prefer a range containing 48 kHz, then f32 samples, then stereo, then fewer channels. The chosen range is clamped to the preferred rate. Supported stream formats are f32, i16, and u16. The requested buffer size is clamped to a reported range; an unknown range uses the system default. A supported range does not guarantee that stream creation will succeed under the current hardware configuration.

The input callback converts interleaved samples to f32, accumulates per-channel meters, and writes complete frames to an `rtrb` SPSC ring buffer. The output callback consumes that buffer through a Rubato variable-ratio sinc resampler. It performs channel mapping and converts to the output format. There is no intermediate worker thread. Device callbacks can have different lengths, and a resampled block can span multiple output callbacks.

DSP buffers and meter accumulators are allocated before streaming starts. Callback paths must avoid application-level allocation, blocking locks, logging, terminal operations, and device reconfiguration. Errors are communicated through a shared flag; the UI performs shutdown.

### Clock drift and latency

Even matching nominal sample rates do not imply a shared hardware clock. The resampler starts with an output-rate/input-rate ratio and adjusts it using ring-buffer occupancy as feedback. A fuller queue lowers that ratio to consume input faster. The normalized occupancy error is clamped, the requested correction is limited to ±0.3%, and the correction is smoothed by a factor of 0.002 per refill. Rubato also ramps ratio changes. These are tuning choices, not a measured guarantee of stability for every device pair.

The fixed resampler output block is 64 frames. Its input requirement varies with the ratio, so each refill queries the required input length. Input scratch space covers the maximum requirement. A 64-tap sinc filter with a 0.9 cutoff and linear interpolation between oversampled filter phases trades filtering cost and quality against latency.

Queue targets use input-frame units: the larger of the requested input callback and the output callback converted to input frames, plus the resampler's maximum input block. Unknown callback sizes are estimated as 512 frames. Capacity is the larger of eight targets and a quarter second of input. Capacity supplies headroom; it does not determine the intended steady-state delay. Observed callback lengths are displayed but do not dynamically retune the target.

Startup waits for the target occupancy. An underrun inserts silence for the remainder of the callback and returns to priming; repeated waiting does not increment the underrun counter. An overflow drops complete incoming frames to preserve channel alignment. Recovery retains the resampler state, so continuity across an interruption is not guaranteed.

Queue telemetry is a snapshot before a refill and excludes resampler, driver, and hardware delays. End-to-end latency requires a physical measurement.

### Shared state and metering

`Stats` contains independent atomic telemetry and control values. Relaxed ordering is sufficient because these values do not publish other memory; ring-buffer synchronization is handled by `rtrb`. UI readings are approximate and are not a coherent snapshot of all counters.

Peak and RMS values are stored as f32 bit patterns in AtomicU32 values. Peak magnitudes are nonnegative, allowing unsigned atomic maximum to retain the largest peak until rendering swaps it to zero. RMS is the latest callback's reading. The terminal event loop polls at 40 ms intervals when idle. Meter rendering consumes the peak hold, so rendering has a deliberate side effect.

Display ballistics run only on the UI thread using elapsed time: the peak envelope falls at 36 dB/s, and the peak marker and numeric reading hold for one second before falling at 18 dB/s. CLIP remains visible for one second after the last clipping peak. Starting a new route resets this state. Every channel's peak is consumed even when the panel cannot fit all channel rows.

### Stream lifecycle

`App` owns an optional `Engine`: selecting devices does not create streams, and removing the engine requests shutdown. Device and buffer changes are restricted to the stopped state. A risky route enters a modal confirmation state before stream creation; device changes are ignored while that confirmation is pending, so approval applies to the displayed pair.

`Engine` is constructed before either stream starts, allowing its destructor to handle partial startup failure. Normal shutdown requests a 10 ms linear fade on the output callback, using output-frame units and the same gain for every channel. The envelope persists across callbacks and reaches silence even without input. After fading, callbacks keep writing silence until the predicted playback delay plus the duration of the final buffer has elapsed. CPAL 0.15's Core Audio backend estimates playback delay from the buffer size, so hardware validation remains necessary.

Only the control thread waits for this acknowledgement, with a 250 ms limit; a stream error skips the wait. An output stream that never successfully started also skips the wait. Shutdown then sets fallback mute, pauses output, and pauses input. Explicit pause is necessary with CPAL 0.15's macOS implementation: its disconnect listener captures a stream clone, so dropping the application's handle alone may leave the stream alive. Pause errors are ignored during destruction; muting provides an additional output safeguard. This workaround stops playback but does not resolve the backend's retained ownership, so repeated reconnects may retain resources. Revisit it when upgrading CPAL.

Stream/DSP errors set an atomic flag. The UI drops the engine and reports a generic error without automatically rerouting. Device removal and sleep/wake recovery require manual refresh and reconnection.

### Feedback heuristic

Name matching is case-insensitive substring matching against microphone and speaker terms. It intentionally does not inspect the signal, device topology, or physical connections. Short terms such as `mic` can produce false positives, and interface names can conceal risky routes. Keep this policy in `safety.rs`; confirmation belongs in the UI. There is no limiter or automatic feedback suppression.

## Validation

Automated tests cover warning-name matches and non-matches, startup priming, underrun recovery, channel separation, the direction of drift correction, shutdown fading across callback boundaries, shutdown without input, playback-drain timing, unsigned output silence, settings persistence and replacement, malformed settings, and restoration across reordered, missing, or duplicate device names. Persistence tests use temporary directories rather than the user's saved settings. They do not exercise physical streams, terminal key delivery, or prove long-term control-loop stability.

Basic operation has been reported by the user. Shutdown fading is covered by automated tests; its audible behavior on physical devices remains to be confirmed.

For hardware validation:

1. Use headphones to check start, stop, mute, meters, and channel mapping. While playing a continuous signal, stop with Space/Esc and quit with q/Ctrl-C; check for clicks at every buffer setting. Repeat start/stop and verify that old streams do not remain audible.
2. Select a microphone/speaker pair and verify that Enter opens confirmation without starting audio. Check cancellation and repeat confirmation on reconnect.
3. Compare 64–512 frame requests and record observed callback lengths and dropout counts.
4. Run independent devices for an extended period, including different nominal sample rates; watch queue occupancy and error counters.
5. Test unplug/replug and sleep/wake, including startup failures and subsequent retries.
6. Measure end-to-end latency using a physical loopback and an impulse or correlated test signal. Record hardware, sample rates, buffer requests, and observed callback lengths with the result.
7. Change both device selections and the buffer size, quit with q or Ctrl-C, and relaunch. Verify the choices return with audio stopped. Disconnect a saved device, relaunch, and check the warning and empty selection; reconnect and press r to restore it.
