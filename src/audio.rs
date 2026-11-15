//! Callback-owned audio processing; only independent atomic values cross into the UI.
//!
//! Device creation and shutdown run on the control thread. Callback code must keep
//! using preallocated storage and must not wait for the terminal or control thread.

use anyhow::{bail, Context, Result};
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Device, Sample, SampleFormat, SizedSample,
};
use rubato::{
    Resampler, SincFixedOut, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use std::sync::{
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed},
    Arc,
};

// This is the resampler's output quantum, independent of device callback lengths.
// A smaller quantum reduces staging delay but increases refill overhead.
const BLOCK: usize = 64;

/// Float bit patterns avoid locks between the input callback and terminal renderer.
/// Peak is a hold consumed by the UI; RMS represents the latest input callback.
#[derive(Default)]
pub struct Meter {
    pub peak: AtomicU32,
    pub rms: AtomicU32,
}
/// Approximate telemetry and independent control flags, not a synchronized snapshot.
/// Relaxed ordering is intentional: no field publishes other memory. The ring
/// buffer provides its own synchronization for the actual audio samples.
#[derive(Default)]
pub struct Stats {
    pub meters: Vec<Meter>,
    pub underruns: AtomicU64,
    pub overflows: AtomicU64,
    pub failed: AtomicBool,
    pub muted: AtomicBool,
    pub queued: AtomicU64,
    pub input_callback: AtomicU64,
    pub output_callback: AtomicU64,
}
pub struct DeviceEntry {
    pub device: Device,
    pub name: String,
}
pub fn devices() -> Result<(Vec<DeviceEntry>, Vec<DeviceEntry>)> {
    let host = cpal::default_host();
    let entry = |device: Device| {
        let name = device.name().unwrap_or_else(|_| "Unknown device".into());
        DeviceEntry { device, name }
    };
    Ok((
        host.input_devices()?.map(entry).collect(),
        host.output_devices()?.map(entry).collect(),
    ))
}

pub struct Engine {
    // Own streams until stopped; callbacks own their preallocated DSP state.
    _input: cpal::Stream,
    _output: cpal::Stream,
    pub stats: Arc<Stats>,
    pub description: String,
    pub input_rate: u32,
}

impl Drop for Engine {
    fn drop(&mut self) {
        // CPAL 0.15's Core Audio disconnect listener retains a Stream clone.
        // Dropping our handles alone therefore does not reliably stop playback.
        // Mute first as a fallback if pausing fails; attempt both pauses even if
        // one fails. This stops playback but cannot break the backend's ownership
        // cycle, which should be revisited on a CPAL upgrade.
        self.stats.muted.store(true, Relaxed);
        let _ = self._output.pause();
        let _ = self._input.pause();
    }
}

fn configuration(
    device: &Device,
    input: bool,
    frames: u32,
) -> Result<(cpal::StreamConfig, SampleFormat)> {
    let ranges: Vec<_> = if input {
        device.supported_input_configs()?.collect()
    } else {
        device.supported_output_configs()?.collect()
    };
    // Rank input and output independently; their clocks and nominal rates need
    // not agree because the output path performs variable-ratio resampling.
    let best = ranges
        .into_iter()
        .filter(|r| {
            matches!(
                r.sample_format(),
                SampleFormat::F32 | SampleFormat::I16 | SampleFormat::U16
            )
        })
        .max_by_key(|r| {
            let rate = r.min_sample_rate().0 <= 48000 && r.max_sample_rate().0 >= 48000;
            (
                rate,
                r.sample_format() == SampleFormat::F32,
                r.channels() == 2,
                std::cmp::Reverse(r.channels()),
            )
        })
        .context("No supported f32/i16/u16 audio configuration")?;
    let rate = 48000u32.clamp(best.min_sample_rate().0, best.max_sample_rate().0);
    let size = match best.buffer_size() {
        cpal::SupportedBufferSize::Range { min, max } => {
            cpal::BufferSize::Fixed(frames.clamp(*min, *max))
        }
        cpal::SupportedBufferSize::Unknown => cpal::BufferSize::Default,
    };
    let format = best.sample_format();
    let mut config = best.with_sample_rate(cpal::SampleRate(rate)).config();
    config.buffer_size = size;
    Ok((config, format))
}

impl Engine {
    pub fn start(input: &Device, output: &Device, frames: u32) -> Result<Self> {
        let (ic, inf) = configuration(input, true, frames)?;
        let (oc, outf) = configuration(output, false, frames)?;
        let channels = ic.channels as usize;
        let out_channels = oc.channels as usize;
        if channels == 0 || out_channels == 0 {
            bail!("Device has no channels");
        }
        // Rubato expresses the ratio as output frames per input frame. Reserve
        // ±1% ratio headroom for drift adjustment around the nominal conversion.
        let ratio = oc.sample_rate.0 as f64 / ic.sample_rate.0 as f64;
        let resampler = SincFixedOut::<f32>::new(
            ratio,
            1.01,
            SincInterpolationParameters {
                sinc_len: 64,
                f_cutoff: 0.9,
                interpolation: SincInterpolationType::Linear,
                oversampling_factor: 128,
                window: WindowFunction::BlackmanHarris2,
            },
            BLOCK,
            channels,
        )?;
        // An unknown device buffer needs a conservative startup estimate. This
        // estimate is not an assertion about the callback sizes CPAL will deliver.
        let callback_frames = |c: &cpal::StreamConfig| match c.buffer_size {
            cpal::BufferSize::Fixed(n) => n as usize,
            _ => 512,
        };
        // Target accommodates a whole callback and a resampling block. Capacity is
        // deliberately larger than the target and does not set steady-state latency.
        let target = callback_frames(&ic)
            .max((callback_frames(&oc) as f64 / ratio).ceil() as usize)
            + resampler.input_frames_max();
        let capacity = (target * 8).max(ic.sample_rate.0 as usize / 4);
        let (producer, consumer) = rtrb::RingBuffer::<f32>::new(capacity * channels);
        let stats = Arc::new(Stats {
            meters: (0..channels).map(|_| Meter::default()).collect(),
            ..Stats::default()
        });
        let input_stream = match inf {
            SampleFormat::F32 => input_stream::<f32>(input, &ic, producer, stats.clone()),
            SampleFormat::I16 => input_stream::<i16>(input, &ic, producer, stats.clone()),
            SampleFormat::U16 => input_stream::<u16>(input, &ic, producer, stats.clone()),
            _ => unreachable!(),
        }?;
        let dsp = OutputDsp {
            // The required input length changes with the ratio. Allocate for the
            // maximum now rather than resize storage on the audio thread.
            input: vec![vec![0.0; resampler.input_frames_max()]; channels],
            output: vec![vec![0.0; BLOCK]; channels],
            resampler,
            consumer,
            channels,
            out_channels,
            target,
            ratio,
            position: BLOCK,
            primed: false,
            correction: 0.0,
        };
        let output_stream = match outf {
            SampleFormat::F32 => output_stream::<f32>(output, &oc, dsp, stats.clone()),
            SampleFormat::I16 => output_stream::<i16>(output, &oc, dsp, stats.clone()),
            SampleFormat::U16 => output_stream::<u16>(output, &oc, dsp, stats.clone()),
            _ => unreachable!(),
        }?;
        let description = format!(
            "{} Hz / {} ch {:?} → {} Hz / {} ch {:?} | requested {:?} / {:?}",
            ic.sample_rate.0,
            channels,
            inf,
            oc.sample_rate.0,
            out_channels,
            outf,
            ic.buffer_size,
            oc.buffer_size
        );
        let engine = Self {
            _input: input_stream,
            _output: output_stream,
            stats,
            description,
            input_rate: ic.sample_rate.0,
        };
        // Establish the cleanup guard before starting either device so a partial
        // startup failure also pauses any stream that has already started.
        engine._output.play()?;
        engine._input.play()?;
        Ok(engine)
    }
}

fn input_stream<T>(
    device: &Device,
    config: &cpal::StreamConfig,
    mut producer: rtrb::Producer<f32>,
    stats: Arc<Stats>,
) -> Result<cpal::Stream>
where
    T: SizedSample,
    f32: cpal::FromSample<T>,
{
    let channels = config.channels as usize;
    let mut peaks = vec![0.0f32; channels];
    let mut squares = vec![0.0f64; channels];
    let errors = stats.clone();
    Ok(device.build_input_stream(
        config,
        move |data: &[T], _| {
            peaks.fill(0.0);
            squares.fill(0.0);
            stats
                .input_callback
                .store((data.len() / channels) as u64, Relaxed);
            for frame in data.chunks_exact(channels) {
                // Dropping an entire frame preserves channel alignment. With one
                // producer, the consumer can only increase free space after this
                // check, so all pushes in the admitted frame have room.
                let space = producer.slots() >= channels;
                if !space {
                    stats.overflows.fetch_add(1, Relaxed);
                }
                for (ch, &sample) in frame.iter().enumerate() {
                    let value = f32::from_sample(sample);
                    peaks[ch] = peaks[ch].max(value.abs());
                    squares[ch] += (value as f64).powi(2);
                    if space {
                        let _ = producer.push(value);
                    }
                }
            }
            let count = (data.len() / channels).max(1) as f64;
            for ch in 0..channels {
                // Nonnegative f32 bit patterns preserve numeric ordering as u32.
                // Atomic max holds transients across callbacks until UI rendering;
                // RMS instead overwrites the previous callback's measurement.
                stats.meters[ch]
                    .peak
                    .fetch_max(peaks[ch].to_bits(), Relaxed);
                stats.meters[ch]
                    .rms
                    .store(((squares[ch] / count).sqrt() as f32).to_bits(), Relaxed);
            }
        },
        move |_| {
            errors.failed.store(true, Relaxed);
        },
        None,
    )?)
}

/// Owned exclusively by the output callback. Scratch buffers are planar even
/// though device data and the ring buffer are interleaved.
struct OutputDsp {
    resampler: SincFixedOut<f32>,
    consumer: rtrb::Consumer<f32>,
    input: Vec<Vec<f32>>,
    output: Vec<Vec<f32>>,
    channels: usize,
    out_channels: usize,
    target: usize, // Input frames, not interleaved samples or output frames.
    ratio: f64,
    position: usize, // Next unread output frame; BLOCK means a refill is needed.
    primed: bool,
    correction: f64,
}
impl OutputDsp {
    fn refill(&mut self, stats: &Stats) -> bool {
        let available = self.consumer.slots() / self.channels;
        stats.queued.store(available as u64, Relaxed);
        if !self.primed {
            // Startup and underrun recovery share the same prefill gate. Waiting
            // here is silence, not another underrun event on every callback.
            if available < self.target {
                return false;
            }
            self.primed = true;
        }
        // A fuller queue needs faster input consumption, hence a smaller out/in ratio.
        // Limit the requested correction to ±0.3% and smooth per refill to avoid
        // chasing callback bursts. Rubato also ramps each ratio update. These
        // tuning constants need long-duration validation on independent devices.
        let error = (available as f64 - self.target as f64) / self.target as f64;
        self.correction += 0.002 * (error.clamp(-1.0, 1.0) * 0.003 - self.correction);
        if self
            .resampler
            .set_resample_ratio(self.ratio * (1.0 - self.correction), true)
            .is_err()
        {
            stats.failed.store(true, Relaxed);
            return false;
        }
        // Query after updating the ratio: a fixed output block does not imply a
        // fixed input requirement. Only this consumer removes queued samples.
        let needed = self.resampler.input_frames_next();
        if available < needed {
            stats.underruns.fetch_add(1, Relaxed);
            self.primed = false;
            return false;
        }
        for i in 0..needed {
            for ch in 0..self.channels {
                self.input[ch][i] = self.consumer.pop().unwrap_or(0.0);
            }
        }
        if self
            .resampler
            .process_into_buffer(&self.input, &mut self.output, None)
            .is_err()
        {
            stats.failed.store(true, Relaxed);
            return false;
        }
        self.position = 0;
        true
    }
}
fn output_stream<T>(
    device: &Device,
    config: &cpal::StreamConfig,
    mut dsp: OutputDsp,
    stats: Arc<Stats>,
) -> Result<cpal::Stream>
where
    T: SizedSample + cpal::FromSample<f32>,
{
    let errors = stats.clone();
    Ok(device.build_output_stream(
        config,
        move |data: &mut [T], _| {
            stats
                .output_callback
                .store((data.len() / dsp.out_channels) as u64, Relaxed);
            // Prefill ensures that a failed refill leaves the rest of the callback
            // silent instead of replaying uninitialized or previous device data.
            data.fill(T::from_sample(0.0));
            if stats.failed.load(Relaxed) {
                return;
            }
            for frame in data.chunks_exact_mut(dsp.out_channels) {
                if dsp.position == BLOCK && !dsp.refill(&stats) {
                    break;
                }
                for (ch, sample) in frame.iter_mut().enumerate() {
                    // Mono duplicates to all outputs. Otherwise preserve channel order;
                    // unmatched outputs are silent, unmatched inputs are not mixed down.
                    // Muting still consumes audio so unmuting does not play a backlog.
                    let source = if dsp.channels == 1 { 0 } else { ch };
                    let value = if stats.muted.load(Relaxed) || source >= dsp.channels {
                        0.0
                    } else {
                        dsp.output[source][dsp.position]
                    };
                    *sample = T::from_sample(value);
                }
                dsp.position += 1;
            }
        },
        move |_| {
            errors.failed.store(true, Relaxed);
        },
        None,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (rtrb::Producer<f32>, OutputDsp, Stats) {
        let resampler = SincFixedOut::<f32>::new(
            1.0,
            1.01,
            SincInterpolationParameters {
                sinc_len: 64,
                f_cutoff: 0.9,
                interpolation: SincInterpolationType::Linear,
                oversampling_factor: 128,
                window: WindowFunction::BlackmanHarris2,
            },
            BLOCK,
            2,
        )
        .unwrap();
        let (producer, consumer) = rtrb::RingBuffer::new(4096);
        let dsp = OutputDsp {
            input: vec![vec![0.0; resampler.input_frames_max()]; 2],
            output: vec![vec![0.0; BLOCK]; 2],
            resampler,
            consumer,
            channels: 2,
            out_channels: 2,
            target: 256,
            ratio: 1.0,
            position: BLOCK,
            primed: false,
            correction: 0.0,
        };
        (producer, dsp, Stats::default())
    }

    #[test]
    fn startup_waits_without_counting_an_underrun() {
        let (_producer, mut dsp, stats) = fixture();
        assert!(!dsp.refill(&stats));
        assert!(!dsp.primed);
        assert_eq!(stats.underruns.load(Relaxed), 0);
    }

    #[test]
    fn resampling_keeps_channels_separate_and_corrects_a_full_queue() {
        let (mut producer, mut dsp, stats) = fixture();
        for _ in 0..1024 {
            producer.push(0.25).unwrap();
            producer.push(-0.25).unwrap();
        }
        for _ in 0..4 {
            assert!(dsp.refill(&stats));
        }
        assert!(dsp.correction > 0.0);
        assert!(dsp.output[0].iter().any(|x| x.abs() > 0.1));
        for i in 0..BLOCK {
            assert!(dsp.output[0][i].is_finite());
            assert!((dsp.output[0][i] + dsp.output[1][i]).abs() < 1e-5);
        }
    }

    #[test]
    fn starvation_counts_once_and_returns_to_priming() {
        let (_producer, mut dsp, stats) = fixture();
        dsp.primed = true;
        assert!(!dsp.refill(&stats));
        assert_eq!(stats.underruns.load(Relaxed), 1);
        assert!(!dsp.primed);
        assert!(!dsp.refill(&stats));
        assert_eq!(stats.underruns.load(Relaxed), 1);
    }
}
