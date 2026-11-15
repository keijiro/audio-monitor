# Audio Monitor

A terminal app for macOS that routes an audio input device to an output device with low latency and shows live input levels. Audio stays stopped until you explicitly start routing.

## Getting started

To run from source, install stable Rust and Xcode Command Line Tools, then run this command in the project directory:

```sh
cargo run --release
```

If you have already built the app, launch it directly:

```sh
./target/release/audio-monitor
```

Select an input and an output using **Tab** and the **arrow keys**, then press **Enter** to start. Press **Space** to stop.

Allow microphone access when macOS asks. If the app cannot receive input, check **System Settings → Privacy & Security → Microphone** and enable access for the terminal app you use to launch it.

## Controls

| Key | Action |
|---|---|
| Tab | Switch between input and output selection |
| ↑ / ↓ | Select a device while stopped |
| Enter | Start routing |
| y | Confirm a feedback warning and start routing |
| n / Esc | Cancel a feedback warning |
| Space / Esc | Stop routing |
| m | Toggle mute; input meters keep running |
| b | Change the requested buffer size while stopped |
| r | Refresh the device lists while stopped |
| q / Ctrl-C | Quit |

Device selection and buffer changes are available only while stopped. Device names appear as reported by macOS.

## Feedback warnings

When the input name looks like a microphone and the output name looks like speakers, the app warns about possible acoustic feedback. **Routing does not start until you press y.** The warning is shown again on each connection attempt.

Detection uses device names, including English and Japanese terms, without distinguishing uppercase and lowercase letters. It can miss microphones or speakers connected through an audio interface, and it can flag harmless routes. There is no automatic feedback suppression or limiter. Use headphones when monitoring a microphone near speakers.

## Buffer size and latency

Press **b** while stopped to cycle through **128 → 256 → 512 → 64 → 128…** frames. The new request takes effect when you start routing again.

Smaller buffers can reduce latency but make audio dropouts more likely. Start with the default **128 frames**, try **64** for lower latency, or increase to **256 / 512** if playback breaks up.

| Buffer size | Duration at 48 kHz |
|---|---:|
| 64 frames | 1.33 ms |
| 128 frames | 2.67 ms |
| 256 frames | 5.33 ms |
| 512 frames | 10.67 ms |

These durations are for one buffer, not the complete input-to-output delay. The app adjusts the request to the device's supported range, or uses the system default if that range is unavailable. The Audio panel shows the stream settings and observed callback sizes.

## Levels and channel routing

Each input channel has a peak meter and an RMS level reading in dBFS. A peak at or above 0 dBFS displays **CLIP**. Muting silences the output without stopping the input meters.

Mono input is copied to every output channel. Otherwise, input channel N goes to output channel N. Extra input channels are discarded and extra output channels stay silent. Stereo input sent to a mono output uses only the left channel.

Audio uses unity gain, with sample-rate conversion as needed to keep the devices synchronized. The output is not guaranteed to be bit-identical to the input.

## Troubleshooting

- **Audio breaks up:** stop routing, increase the buffer size with **b**, and start again. Increasing underrun or dropped-input counters indicates that the app is not keeping up reliably.
- **A device is missing or disconnected:** stop, press **r**, select the devices again, and restart. Follow the same steps after waking from sleep if routing fails.
- **Routing fails to start:** check microphone permission and the devices' settings in Audio MIDI Setup. Try a different buffer size.
- **The Queue value seems too small to explain the delay:** Queue shows only buffered input audio. Hardware, drivers, and resampling add more delay.

A stream error stops routing. The app does not automatically switch to another device.
