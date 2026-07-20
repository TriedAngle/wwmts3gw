use std::io::Cursor;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

pub const SAMPLE_RATE: usize = 48_000;

/// Decodes an audio file (any format symphonia knows), downmixes to mono,
/// resamples to 48 kHz and applies the volume. Returns raw PCM samples.
pub fn decode_clip(path: &Path, volume: f32) -> Result<Vec<f32>> {
    let data = std::fs::read(path)
        .with_context(|| format!("failed to read audio file: {}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default());

    let probed = symphonia::default::get_probe()
        .format(
            &Hint::new(),
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| anyhow!("unsupported audio format: {e}"))?;

    let mut format = probed.format;
    let track = format
        .tracks()
        .first()
        .ok_or_else(|| anyhow!("no audio track found"))?
        .clone();
    let track_id = track.id;
    let file_sample_rate = track
        .codec_params
        .sample_rate
        .ok_or_else(|| anyhow!("unknown sample rate"))?;
    let channels = track.codec_params.channels.map(|c| c.count()).unwrap_or(1);

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| anyhow!("unsupported codec: {e}"))?;

    let mut samples: Vec<f32> = Vec::new();

    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(symphonia::core::errors::Error::IoError(ref e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break
            }
            Err(e) => return Err(anyhow!("decode error: {e}")),
        };

        if packet.track_id() != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            // Malformed frames (common at the start of MP3 streams) are skippable.
            Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
            Err(e) => return Err(anyhow!("decode error: {e}")),
        };

        let spec = *decoded.spec();
        let mut buf = SampleBuffer::<f32>::new(decoded.frames() as u64, spec);
        buf.copy_interleaved_ref(decoded);

        if channels == 1 {
            samples.extend_from_slice(buf.samples());
        } else {
            for chunk in buf.samples().chunks(channels) {
                samples.push(chunk.iter().sum::<f32>() / channels as f32);
            }
        }
    }

    if file_sample_rate != SAMPLE_RATE as u32 {
        samples = resample(&samples, file_sample_rate, SAMPLE_RATE as u32);
    }

    for s in &mut samples {
        *s = (*s * volume).clamp(-1.0, 1.0);
    }

    Ok(samples)
}

/// Linear-interpolation resampler. Good enough for short voice announcements;
/// swap in a windowed-sinc resampler (e.g. `rubato`) if music clips sound dull.
fn resample(samples: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
    let out_len = samples.len() as u64 * to_rate as u64 / from_rate as u64;
    let step = from_rate as f64 / to_rate as f64;
    (0..out_len)
        .map(|i| {
            let pos = i as f64 * step;
            let idx = pos as usize;
            let frac = (pos - idx as f64) as f32;
            let a = samples[idx];
            let b = *samples.get(idx + 1).unwrap_or(&a);
            a + (b - a) * frac
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resample_interpolates_linearly() {
        // Doubling the rate on [0, 1] yields the halfway points.
        assert_eq!(resample(&[0.0, 1.0], 1, 2), vec![0.0, 0.5, 1.0, 1.0]);
        assert_eq!(resample(&[0.0, 1.0, 0.0], 2, 1), vec![0.0]);
    }

    #[test]
    fn decodes_a_44100_hz_wav() {
        let sample_rate = 44_100u32;
        let samples: Vec<i16> = (0..sample_rate as usize / 2)
            .map(|i| {
                let t = i as f32 / sample_rate as f32;
                ((t * 440.0 * std::f32::consts::TAU).sin() * 8000.0) as i16
            })
            .collect();

        let path = std::env::temp_dir().join("wwmts3gw-test-44100.wav");
        std::fs::write(&path, wav_bytes(sample_rate, &samples)).unwrap();

        let pcm = decode_clip(&path, 1.0).unwrap();
        // 0.5 s at 44.1 kHz resamples to exactly 24_000 samples at 48 kHz.
        assert_eq!(pcm.len(), 24_000);
    }

    fn wav_bytes(sample_rate: u32, samples: &[i16]) -> Vec<u8> {
        let data_len = samples.len() as u32 * 2;
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&1u16.to_le_bytes()); // mono
        out.extend_from_slice(&sample_rate.to_le_bytes());
        out.extend_from_slice(&(sample_rate * 2).to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes()); // block align
        out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        for s in samples {
            out.extend_from_slice(&s.to_le_bytes());
        }
        out
    }
}
