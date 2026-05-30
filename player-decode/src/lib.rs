//! Decoder integration (Symphonia skeleton)
// Decoder pipeline: safe defaults and feature-gated Symphonia integration.

pub mod probe {
    /// Fast magic-byte probe (first 512 bytes) for some common formats.
    pub fn probe_bytes(buf: &[u8]) -> Option<&'static str> {
        if buf.len() >= 12 {
            // RIFF/WAVE
            if &buf[0..4] == b"RIFF" && &buf[8..12] == b"WAVE" {
                return Some("wav");
            }
            // FLAC
            if &buf[0..4] == b"fLaC" {
                return Some("flac");
            }
            // Ogg (Vorbis/Opus)
            if &buf[0..4] == b"OggS" {
                return Some("ogg");
            }
            // MP3 (ID3 or frame sync 0xFFFB)
            if &buf[0..3] == b"ID3" {
                return Some("mp3");
            }
        }
        // Fallback: check for ftyp in MP4/M4A
        if buf.len() >= 12 && &buf[4..8] == b"ftyp" {
            return Some("mp4");
        }
        None
    }
}

pub mod ringbuf {
    use std::collections::VecDeque;

    /// Simple ring buffer holding interleaved f32 samples.
    pub struct RingBuffer {
        buf: VecDeque<f32>,
        capacity_frames: usize,
        channels: usize,
    }

    impl RingBuffer {
        pub fn new(capacity_seconds: f32, sample_rate: u32, channels: usize) -> Self {
            let capacity_frames = (capacity_seconds * sample_rate as f32) as usize;
            Self {
                buf: VecDeque::with_capacity(capacity_frames * channels),
                capacity_frames,
                channels,
            }
        }

        pub fn push_interleaved(&mut self, samples: &[f32]) {
            for &s in samples {
                if self.buf.len() >= self.capacity_frames * self.channels {
                    let _ = self.buf.pop_front();
                }
                self.buf.push_back(s);
            }
        }

        pub fn pop_frames(&mut self, frames: usize) -> Vec<f32> {
            let mut out = Vec::with_capacity(frames * self.channels);
            for _ in 0..(frames * self.channels) {
                if let Some(v) = self.buf.pop_front() {
                    out.push(v);
                } else {
                    break;
                }
            }
            out
        }

        pub fn len_frames(&self) -> usize {
            self.buf.len() / self.channels
        }
    }
}

/// Decoder trait: abstract over concrete decoder backends.
pub trait Decoder: Send {
    /// Fill the provided buffer with interleaved f32 samples. Returns number of samples written.
    fn next_samples(&mut self, out: &mut Vec<f32>, max_samples: usize) -> anyhow::Result<usize>;

    /// Current sample rate in Hz.
    fn sample_rate(&self) -> u32;

    /// Number of channels.
    fn channels(&self) -> u16;
}

#[cfg(feature = "symphonia")]
pub mod symphonia_impl {
    //! Symphonia-based decoder implementation.
    use super::Decoder;
    use anyhow::{anyhow, Result};
    use symphonia::core::audio::{AudioBufferRef, SampleBuffer, Signal};
    use symphonia::core::codecs::DecoderOptions;
    use symphonia::core::errors::Error as SymphError;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::default::{get_codecs, get_probe};

    pub struct SymphoniaDecoder {
        sample_rate: u32,
        channels: u16,
        // format and decoder
        format: Box<dyn symphonia::core::formats::FormatReader>,
        decoder: Box<dyn symphonia::core::codecs::Decoder>,
        track_id: u32,
    }

    impl SymphoniaDecoder {
        pub fn open_path<P: AsRef<std::path::Path>>(path: P) -> Result<Self> {
            let file = std::fs::File::open(path.as_ref())?;
            let mss = MediaSourceStream::new(Box::new(file), Default::default());
            let mut hint = symphonia::core::probe::Hint::new();
            // Try to hint by extension when possible
            if let Some(ext) = path.as_ref().extension().and_then(|s| s.to_str()) {
                hint.with_extension(ext);
            }

            let probe = get_probe()
                .format(
                    &hint,
                    mss,
                    &FormatOptions::default(),
                    &MetadataOptions::default(),
                )
                .map_err(|e| anyhow!("probe error: {:?}", e))?;
            let format = probe.format;

            // Select the first audio track and clone codec params to avoid borrow issues
            let track0 = format
                .tracks()
                .get(0)
                .ok_or_else(|| anyhow!("no audio track found"))?;
            let codec_params = track0.codec_params.clone();
            let track_id = track0.id;
            let dec_cfg = DecoderOptions::default();
            let decoder = get_codecs()
                .make(&codec_params, &dec_cfg)
                .map_err(|e| anyhow!("decoder make error: {:?}", e))?;

            let sr = codec_params
                .sample_rate
                .ok_or_else(|| anyhow!("unknown sample rate"))? as u32;
            let ch = codec_params.channels.map(|c| c.count() as u16).unwrap_or(2);

            Ok(Self {
                sample_rate: sr,
                channels: ch,
                format,
                decoder,
                track_id,
            })
        }
    }

    impl Decoder for SymphoniaDecoder {
        fn next_samples(&mut self, out: &mut Vec<f32>, max_samples: usize) -> Result<usize> {
            out.clear();
            // Read packets until we produce samples or reach EOF
            loop {
                match self.format.next_packet() {
                    Ok(pkt) => {
                        if pkt.track_id() != self.track_id {
                            continue;
                        }
                        match self.decoder.decode(&pkt) {
                            Ok(audio_buf) => {
                                // Convert audio_buf (AudioBufferRef) into interleaved f32 via SampleBuffer
                                match audio_buf {
                                    AudioBufferRef::U8(buf) => {
                                        let mut sb = SampleBuffer::<f32>::new(
                                            buf.frames() as u64,
                                            buf.spec().clone(),
                                        );
                                        sb.copy_interleaved_ref(AudioBufferRef::U8(buf));
                                        out.extend_from_slice(sb.samples());
                                    }
                                    AudioBufferRef::U16(buf) => {
                                        let mut sb = SampleBuffer::<f32>::new(
                                            buf.frames() as u64,
                                            buf.spec().clone(),
                                        );
                                        sb.copy_interleaved_ref(AudioBufferRef::U16(buf));
                                        out.extend_from_slice(sb.samples());
                                    }
                                    AudioBufferRef::U24(buf) => {
                                        let mut sb = SampleBuffer::<f32>::new(
                                            buf.frames() as u64,
                                            buf.spec().clone(),
                                        );
                                        sb.copy_interleaved_ref(AudioBufferRef::U24(buf));
                                        out.extend_from_slice(sb.samples());
                                    }
                                    AudioBufferRef::U32(buf) => {
                                        let mut sb = SampleBuffer::<f32>::new(
                                            buf.frames() as u64,
                                            buf.spec().clone(),
                                        );
                                        sb.copy_interleaved_ref(AudioBufferRef::U32(buf));
                                        out.extend_from_slice(sb.samples());
                                    }
                                    AudioBufferRef::S8(buf) => {
                                        let mut sb = SampleBuffer::<f32>::new(
                                            buf.frames() as u64,
                                            buf.spec().clone(),
                                        );
                                        sb.copy_interleaved_ref(AudioBufferRef::S8(buf));
                                        out.extend_from_slice(sb.samples());
                                    }
                                    AudioBufferRef::S16(buf) => {
                                        let mut sb = SampleBuffer::<f32>::new(
                                            buf.frames() as u64,
                                            buf.spec().clone(),
                                        );
                                        sb.copy_interleaved_ref(AudioBufferRef::S16(buf));
                                        out.extend_from_slice(sb.samples());
                                    }
                                    AudioBufferRef::S24(buf) => {
                                        let mut sb = SampleBuffer::<f32>::new(
                                            buf.frames() as u64,
                                            buf.spec().clone(),
                                        );
                                        sb.copy_interleaved_ref(AudioBufferRef::S24(buf));
                                        out.extend_from_slice(sb.samples());
                                    }
                                    AudioBufferRef::S32(buf) => {
                                        let mut sb = SampleBuffer::<f32>::new(
                                            buf.frames() as u64,
                                            buf.spec().clone(),
                                        );
                                        sb.copy_interleaved_ref(AudioBufferRef::S32(buf));
                                        out.extend_from_slice(sb.samples());
                                    }
                                    AudioBufferRef::F32(buf) => {
                                        let mut sb = SampleBuffer::<f32>::new(
                                            buf.frames() as u64,
                                            buf.spec().clone(),
                                        );
                                        sb.copy_interleaved_ref(AudioBufferRef::F32(buf));
                                        out.extend_from_slice(sb.samples());
                                    }
                                    AudioBufferRef::F64(buf) => {
                                        let mut sb = SampleBuffer::<f32>::new(
                                            buf.frames() as u64,
                                            buf.spec().clone(),
                                        );
                                        sb.copy_interleaved_ref(AudioBufferRef::F64(buf));
                                        out.extend_from_slice(sb.samples());
                                    }
                                }
                                if out.len() >= max_samples {
                                    return Ok(out.len());
                                }
                            }
                            Err(SymphError::DecodeError(_)) => continue,
                            Err(e) => return Err(anyhow!("decode error: {:?}", e)),
                        }
                    }
                    Err(err) => {
                        // EOF or fatal error
                        return match err {
                            SymphError::IoError(_) | SymphError::ResetRequired => Ok(out.len()),
                            _ => Err(anyhow!("format read error: {:?}", err)),
                        };
                    }
                }
            }
        }

        fn sample_rate(&self) -> u32 {
            self.sample_rate
        }

        fn channels(&self) -> u16 {
            self.channels
        }
    }
}
