use std::fs::File;
use std::io::Write;
fn main() {
    let sr: u32 = 44100;
    let dur: f32 = 1.0;
    let n = (sr as f32 * dur) as u32;
    let freq: f32 = 440.0;
    let mut f = File::create("test.wav").expect("create");
    f.write_all(b"RIFF").unwrap();
    let data_size = n * 2;
    let file_size = 36 + data_size;
    f.write_all(&file_size.to_le_bytes()).unwrap();
    f.write_all(b"WAVEfmt ").unwrap();
    f.write_all(&16u32.to_le_bytes()).unwrap();
    f.write_all(&1u16.to_le_bytes()).unwrap();
    f.write_all(&1u16.to_le_bytes()).unwrap();
    f.write_all(&sr.to_le_bytes()).unwrap();
    f.write_all(&(sr * 2).to_le_bytes()).unwrap();
    f.write_all(&2u16.to_le_bytes()).unwrap();
    f.write_all(&16u16.to_le_bytes()).unwrap();
    f.write_all(b"data").unwrap();
    f.write_all(&data_size.to_le_bytes()).unwrap();
    for i in 0..n {
        let t = i as f32 / sr as f32;
        let s = ( (2.0 * std::f32::consts::PI * freq * t).sin() * 0.5 * 32767.0) as i16;
        f.write_all(&s.to_le_bytes()).unwrap();
    }
    println!("WAV written: test.wav ({} samples)", n);
}
