use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    use clap::Parser;
    #[cfg(feature = "test-sinks")]
    use player_audio::AudioSink;

    #[derive(Parser)]
    struct Opts {
        /// Play a generated sine tone instead of a file
        #[arg(long)]
        sine: bool,

        /// Frequency for sine (Hz)
        #[arg(long, default_value_t = 440.0)]
        freq: f32,

        /// File to play (when not using --sine)
        file: Option<PathBuf>,
    }

    let opts = Opts::parse();

    if opts.sine {
        println!("Playing sine {} Hz (test sink)", opts.freq);
        #[cfg(feature = "test-sinks")]
        {
            let mut sink = player_audio::test_sinks::SineSink::new(opts.freq, 48000, 2);
            sink.start()?;
            // Run for a short burst
            let buf = vec![0.0f32; 48000 * 2 / 10];
            for _ in 0..10 {
                let _ = sink.write(&buf)?;
            }
            sink.stop()?;
        }
        #[cfg(not(feature = "test-sinks"))]
        {
            println!("Enable the `test-sinks` feature in `player-audio` to run the sine test.");
        }
        return Ok(());
    }

    if let Some(path) = opts.file {
        println!("Attempting to play file: {}", path.display());
        #[cfg(feature = "symphonia")]
        {
            use player_audio::test_sinks::NullSink;
            use player_decode::symphonia_impl::SymphoniaDecoder;
            use player_decode::Decoder;

            let mut dec = SymphoniaDecoder::open_path(&path)?;
            let mut sink = NullSink::new();
            sink.start()?;
            let mut samples = Vec::new();
            loop {
                let n = dec.next_samples(
                    &mut samples,
                    (dec.sample_rate() as usize) * (dec.channels() as usize),
                )?;
                if n == 0 {
                    break;
                }
                let _ = sink.write(&samples)?;
            }
            sink.stop()?;
        }
        #[cfg(feature = "wav")]
        {
            use hound;
            use player_audio::test_sinks::NullSink;

            let mut reader = hound::WavReader::open(&path)?;
            let spec = reader.spec();
            let sr = spec.sample_rate as u32;
            let channels = spec.channels as u16;
            let mut sink = NullSink::new();
            sink.start()?;
            let mut samples_f32 = Vec::new();
            for s in reader.samples::<i16>() {
                let v = s? as f32 / 32768.0;
                samples_f32.push(v);
            }
            // Optionally resample to sink rate (use resample feature in player-audio)
            let mut resampled = Vec::new();
            let mut r = player_audio::resample::Resampler::new(
                sr as usize,
                48000usize,
                channels as usize,
                player_audio::resample::Quality::Balanced,
            )?;
            r.process(&samples_f32, &mut resampled)?;
            let _ = sink.write(&resampled)?;
            sink.stop()?;
        }
        #[cfg(not(feature = "wav"))]
        {
            println!("Enable the `wav` feature in `player-cli` to decode WAV files for tests.");
        }
    } else {
        println!("No file provided. Use --sine or pass a file path.");
    }

    Ok(())
}
