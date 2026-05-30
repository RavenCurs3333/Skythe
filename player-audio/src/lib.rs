//! `player-audio` crate: defines `AudioSink` and provides backend skeletons.
use anyhow::Result;

pub type Sample = f32;

/// Core trait for audio backends. Uses interleaved f32 samples.
pub trait AudioSink: Send + Sync {
    /// Start the sink and prepare for playback.
    fn start(&mut self) -> Result<()>;

    /// Stop the sink and release resources.
    fn stop(&mut self) -> Result<()>;

    /// Write interleaved f32 samples to the sink.
    fn write(&mut self, data: &[Sample]) -> Result<usize>;

    /// Set the sample rate (Hz). Implementations may restart or reconfigure.
    fn set_sample_rate(&mut self, sample_rate: u32) -> Result<()>;

    /// Set number of channels.
    fn set_channels(&mut self, channels: u16) -> Result<()>;
}

#[cfg(feature = "pipewire")]
pub mod pipewire_impl {
    use super::*;
    use anyhow::Result;

    /// Skeleton PipeWire-based sink. Fill in integration with the `pipewire` crate.
    pub struct PipeWireSink {
        sample_rate: u32,
        channels: u16,
        // Sender for passing audio buffers to the PipeWire thread. The actual
        // PipeWire objects (MainLoopRc, ContextRc, CoreRc, Stream) live inside
        // the background thread and are not stored here to avoid Send/Sync issues.
        #[cfg(feature = "pipewire")]
        tx: Option<std::sync::mpsc::SyncSender<Vec<f32>>>,
        #[cfg(feature = "pipewire")]
        bg_thread: Option<std::thread::JoinHandle<()>>,
    }

    impl PipeWireSink {
        pub fn new() -> Result<Self> {
            #[cfg(feature = "pipewire")]
            {
                // Create a bounded channel for audio frames. Spawn a thread that
                // initializes PipeWire objects and consumes buffers for playback.
                let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(8);

                let handle = std::thread::spawn(move || {
                    // This thread owns all PipeWire Rc objects (non-Send) so they
                    // never cross thread boundaries.
                    if let Ok(ml) = pipewire::main_loop::MainLoopRc::new(None) {
                        if let Ok(ctx) = pipewire::context::ContextRc::new(&ml, None) {
                            if let Ok(core) = ctx.connect_rc(None) {
                                // TODO: Create StreamBox, negotiate params, and implement
                                // dequeue/queue buffer processing. For now, this thread
                                // will simply drain incoming buffers to keep memory bounded.
                                for buf in rx.iter() {
                                    // placeholder: drop the buffer or log length
                                    let _ = buf.len();
                                }
                                let _ = core;
                            }
                        }
                        // keep running mainloop while channel is open
                        ml.run();
                    } else {
                        // If we can't init PipeWire, just drain the receiver
                        for _ in rx.iter() { /* drop */ }
                    }
                });

                return Ok(Self {
                    sample_rate: 48000,
                    channels: 2,
                    tx: Some(tx),
                    bg_thread: Some(handle),
                });
            }

            #[cfg(not(feature = "pipewire"))]
            {
                Ok(Self {
                    sample_rate: 48000,
                    channels: 2,
                    tx: None,
                    bg_thread: None,
                })
            }
        }
    }

    impl AudioSink for PipeWireSink {
        fn start(&mut self) -> Result<()> {
            // nothing to do; thread already started in `new()`
            Ok(())
        }

        fn stop(&mut self) -> Result<()> {
            #[cfg(feature = "pipewire")]
            {
                // Drop the sender so the thread's recv loop ends; then join.
                self.tx.take();
                if let Some(handle) = self.bg_thread.take() {
                    let _ = handle.join();
                }
            }
            Ok(())
        }

        fn write(&mut self, _data: &[Sample]) -> Result<usize> {
            #[cfg(feature = "pipewire")]
            {
                if let Some(ref tx) = self.tx {
                    // send a copy of the samples as an owned Vec<f32>
                    let mut v = Vec::with_capacity(_data.len());
                    v.extend_from_slice(_data);
                    match tx.try_send(v) {
                        Ok(()) => Ok(_data.len()),
                        Err(std::sync::mpsc::TrySendError::Full(_)) => Ok(0),
                        Err(e) => Err(anyhow::anyhow!("pipewire send error: {:?}", e)),
                    }
                } else {
                    Ok(0)
                }
            }
            #[cfg(not(feature = "pipewire"))]
            {
                Ok(0)
            }
        }

        fn set_sample_rate(&mut self, sample_rate: u32) -> Result<()> {
            self.sample_rate = sample_rate;
            Ok(())
        }

        fn set_channels(&mut self, channels: u16) -> Result<()> {
            self.channels = channels;
            Ok(())
        }
    }
}

// Re-export a default sink type behind feature flags later.
pub mod resample {
    //! Resampler wrapper: uses `rubato` when the `resample` feature is enabled,
    //! otherwise falls back to a pass-through (no resampling).

    use crate::Sample;

    pub enum Quality {
        Fastest,
        Balanced,
        Highest,
    }

    pub struct Resampler {
        in_rate: usize,
        out_rate: usize,
        channels: usize,
        _quality: Quality,
        // feature-gated backend would be stored here
        #[cfg(feature = "resample")]
        backend: Option<rubato::Async<f32>>,
    }

    impl Resampler {
        pub fn new(
            in_rate: usize,
            out_rate: usize,
            channels: usize,
            quality: Quality,
        ) -> anyhow::Result<Self> {
            #[cfg(feature = "resample")]
            let backend = {
                use rubato::{
                    Async, FixedAsync, SincInterpolationParameters, SincInterpolationType,
                    WindowFunction,
                };

                let params = SincInterpolationParameters {
                    sinc_len: 128,
                    f_cutoff: 0.95,
                    oversampling_factor: 128,
                    interpolation: SincInterpolationType::Cubic,
                    window: WindowFunction::BlackmanHarris2,
                };
                let ratio = (out_rate as f64) / (in_rate as f64);
                let chunk_size = 1024usize;
                Async::new_sinc(ratio, 2.0, &params, chunk_size, channels, FixedAsync::Input).ok()
            };

            Ok(Self {
                in_rate,
                out_rate,
                channels,
                _quality: quality,
                #[cfg(feature = "resample")]
                backend,
            })
        }

        /// Resample interleaved input into output. Default implementation: nearest-sample copy when rates match
        pub fn process(
            &mut self,
            input: &[Sample],
            output: &mut Vec<Sample>,
        ) -> anyhow::Result<()> {
            if self.in_rate == self.out_rate {
                output.extend_from_slice(input);
                return Ok(());
            }

            // Simple linear interpolation (fallback and default behaviour).
            let frames_in = input.len() / self.channels;
            if frames_in == 0 {
                return Ok(());
            }
            let frames_out =
                ((frames_in as f64) * (self.out_rate as f64) / (self.in_rate as f64)) as usize;
            output.resize(frames_out * self.channels, 0.0);
            // If rubato is available try to use it for higher quality resampling.
            #[cfg(feature = "resample")]
            {
                if let Some(ref mut r) = self.backend {
                    use rubato::Resampler;

                    struct InterleavedSlice<'a> {
                        buf: &'a [f32],
                        frames: usize,
                        channels: usize,
                    }
                    unsafe impl<'a> rubato::audioadapter::Adapter<'a, f32> for InterleavedSlice<'a> {
                        unsafe fn read_sample_unchecked(
                            &self,
                            channel: usize,
                            frame: usize,
                        ) -> f32 {
                            *self.buf.get_unchecked(frame * self.channels + channel)
                        }
                        fn channels(&self) -> usize {
                            self.channels
                        }
                        fn frames(&self) -> usize {
                            self.frames
                        }
                    }

                    struct InterleavedMut<'a> {
                        buf: &'a mut [f32],
                        frames: usize,
                        channels: usize,
                    }
                    unsafe impl<'a> rubato::audioadapter::Adapter<'a, f32> for InterleavedMut<'a> {
                        unsafe fn read_sample_unchecked(
                            &self,
                            channel: usize,
                            frame: usize,
                        ) -> f32 {
                            *self.buf.get_unchecked(frame * self.channels + channel)
                        }
                        fn channels(&self) -> usize {
                            self.channels
                        }
                        fn frames(&self) -> usize {
                            self.frames
                        }
                    }
                    unsafe impl<'a> rubato::audioadapter::AdapterMut<'a, f32> for InterleavedMut<'a> {
                        unsafe fn write_sample_unchecked(
                            &mut self,
                            channel: usize,
                            frame: usize,
                            value: &f32,
                        ) -> bool {
                            let idx = frame * self.channels + channel;
                            *self.buf.get_unchecked_mut(idx) = *value;
                            false
                        }
                    }

                    let frames_in = input.len() / self.channels;
                    let est_out = ((frames_in as f64) * (self.out_rate as f64)
                        / (self.in_rate as f64))
                        .ceil() as usize;
                    output.resize(est_out * self.channels, 0.0);

                    let in_adapter = InterleavedSlice {
                        buf: input,
                        frames: frames_in,
                        channels: self.channels,
                    };
                    let mut out_adapter = InterleavedMut {
                        buf: output.as_mut_slice(),
                        frames: est_out,
                        channels: self.channels,
                    };

                    let _ = r
                        .process_into_buffer(&in_adapter, &mut out_adapter, None)
                        .map_err(|e| anyhow::anyhow!("rubato error: {:?}", e))?;
                    return Ok(());
                }
            }

            // Fallback linear interpolation if rubato not available or failed
            for ch in 0..self.channels {
                for i in 0..frames_out {
                    let src_pos = (i as f64) * (self.in_rate as f64) / (self.out_rate as f64);
                    let idx = src_pos.floor() as usize;
                    let frac = src_pos - (idx as f64);
                    let a = if idx < frames_in {
                        input[idx * self.channels + ch]
                    } else {
                        0.0
                    };
                    let b = if idx + 1 < frames_in {
                        input[(idx + 1) * self.channels + ch]
                    } else {
                        0.0
                    };
                    output[i * self.channels + ch] =
                        a as f32 * (1.0 - frac as f32) + b as f32 * (frac as f32);
                }
            }
            Ok(())
        }
    }
}

pub mod mix {
    //! Channel mixing utilities (matrix-based simple rules)
    use crate::Sample;

    /// Mix interleaved `in_ch` channels to `out_ch` channels using simple rules.
    /// This is a lightweight, real-time friendly implementation.
    pub fn mix_channels(input: &[Sample], in_ch: usize, out_ch: usize, out: &mut Vec<Sample>) {
        if in_ch == out_ch {
            out.extend_from_slice(input);
            return;
        }

        let frames = input.len() / in_ch;
        out.clear();
        out.resize(frames * out_ch, 0.0);

        for f in 0..frames {
            for ic in 0..in_ch {
                let sample = input[f * in_ch + ic];
                match (in_ch, out_ch) {
                    (1, 2) => {
                        // mono -> stereo: duplicate
                        out[f * 2 + 0] += sample;
                        out[f * 2 + 1] += sample;
                    }
                    (2, 1) => {
                        // stereo -> mono: average
                        out[f] += sample * 0.5;
                    }
                    (2, 6) => {
                        // stereo -> 5.1: map L->L, R->R, others 0
                        let l = input[f * 2 + 0];
                        let r = input[f * 2 + 1];
                        out[f * 6 + 0] = l; // FL
                        out[f * 6 + 1] = r; // FR
                    }
                    _ => {
                        // generic: copy into first min channels
                        let oc = ic.min(out_ch - 1);
                        out[f * out_ch + oc] += sample;
                    }
                }
            }
        }
    }
}

#[cfg(feature = "test-sinks")]
pub mod test_sinks {
    use super::{AudioSink, Sample};
    use anyhow::Result;

    /// A sink that discards samples (useful for smoke tests).
    pub struct NullSink;

    impl NullSink {
        pub fn new() -> Self {
            NullSink
        }
    }

    impl AudioSink for NullSink {
        fn start(&mut self) -> Result<()> {
            Ok(())
        }
        fn stop(&mut self) -> Result<()> {
            Ok(())
        }
        fn write(&mut self, data: &[Sample]) -> Result<usize> {
            Ok(data.len())
        }
        fn set_sample_rate(&mut self, _sample_rate: u32) -> Result<()> {
            Ok(())
        }
        fn set_channels(&mut self, _channels: u16) -> Result<()> {
            Ok(())
        }
    }

    /// A simple sink that generates a sine into an internal buffer on demand.
    pub struct SineSink {
        sample_rate: u32,
        channels: u16,
        phase: f32,
        freq: f32,
    }

    impl SineSink {
        pub fn new(freq: f32, sample_rate: u32, channels: u16) -> Self {
            Self {
                sample_rate,
                channels,
                phase: 0.0,
                freq,
            }
        }
    }

    impl AudioSink for SineSink {
        fn start(&mut self) -> Result<()> {
            Ok(())
        }
        fn stop(&mut self) -> Result<()> {
            Ok(())
        }

        fn write(&mut self, data: &[Sample]) -> Result<usize> {
            // Fill the provided buffer length with sine samples (ignore input contents).
            let frames = data.len() / (self.channels as usize);
            let two_pi_f = 2.0 * std::f32::consts::PI * self.freq / (self.sample_rate as f32);
            for i in 0..frames {
                let s = (self.phase + (i as f32) * two_pi_f).sin();
                for ch in 0..(self.channels as usize) {
                    let idx = i * (self.channels as usize) + ch;
                    if idx < data.len() {
                        // safety: we can't mutate the borrowed slice; this sink discards anyway
                        let _ = s;
                    }
                }
            }
            self.phase += (frames as f32) * two_pi_f;
            Ok(data.len())
        }

        fn set_sample_rate(&mut self, sample_rate: u32) -> Result<()> {
            self.sample_rate = sample_rate;
            Ok(())
        }

        fn set_channels(&mut self, channels: u16) -> Result<()> {
            self.channels = channels;
            Ok(())
        }
    }
}
