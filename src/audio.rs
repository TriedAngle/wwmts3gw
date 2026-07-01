use std::io::Cursor;

use anyhow::{anyhow, Context, Result};
use audiopus::coder::Encoder;
use audiopus::{Application, Channels, SampleRate};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use tokio::sync::mpsc;
use tokio::time::{self, Duration};
use tsproto_packets::packets::{AudioData, CodecType, OutAudio, OutPacket};

use crate::PlaybackTarget;

const SAMPLE_RATE: usize = 48_000;
const FRAME_MS: u64 = 20;
const SAMPLES_PER_FRAME: usize = SAMPLE_RATE / 50;
const MAX_OPUS_FRAME_SIZE: usize = 1_275;

pub async fn play_audio_once(
    audio_file: &std::path::Path,
    volume: f32,
    target: &PlaybackTarget,
    tx: &mpsc::Sender<OutPacket>,
) -> Result<()> {
    let data = tokio::fs::read(audio_file)
        .await
        .with_context(|| format!("failed to read audio file: {}", audio_file.display()))?;

    let cursor = Cursor::new(data);
    let mss = MediaSourceStream::new(Box::new(cursor), Default::default());

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

    if file_sample_rate != SAMPLE_RATE as u32 {
        anyhow::bail!(
            "audio file is {} Hz; expected {} Hz",
            file_sample_rate,
            SAMPLE_RATE
        );
    }

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

        let decoded = decoder
            .decode(&packet)
            .map_err(|e| anyhow!("decode error: {e}"))?;

        let spec = *decoded.spec();
        let frames = decoded.frames() as u64;
        let mut buf = SampleBuffer::<f32>::new(frames, spec);
        buf.copy_interleaved_ref(decoded);

        if channels == 1 {
            samples.extend_from_slice(buf.samples());
        } else {
            for chunk in buf.samples().chunks(channels) {
                samples.push(chunk.iter().sum::<f32>() / channels as f32);
            }
        }
    }

    let encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip)
        .map_err(|e| anyhow!("failed to create Opus encoder: {e:?}"))?;

    let mut pcm_frame = vec![0_f32; SAMPLES_PER_FRAME];
    let mut opus_frame = [0_u8; MAX_OPUS_FRAME_SIZE];
    let mut ticker = time::interval(Duration::from_millis(FRAME_MS));
    let mut idx = 0;

    while idx + SAMPLES_PER_FRAME <= samples.len() {
        ticker.tick().await;

        for (out, &inp) in pcm_frame.iter_mut().zip(&samples[idx..]) {
            *out = (inp * volume).clamp(-1.0, 1.0);
        }

        let len = encoder
            .encode_float(&pcm_frame, &mut opus_frame)
            .map_err(|e| anyhow!("failed to encode Opus frame: {e:?}"))?;

        tx.send(make_packet(target, &opus_frame[..len]))
            .await
            .map_err(|_| anyhow!("TeamSpeak sender stopped"))?;

        idx += SAMPLES_PER_FRAME;
    }

    let remaining = samples.len() - idx;
    if remaining > 0 {
        pcm_frame.fill(0.0);
        for (out, &inp) in pcm_frame.iter_mut().zip(&samples[idx..]) {
            *out = (inp * volume).clamp(-1.0, 1.0);
        }

        let len = encoder
            .encode_float(&pcm_frame, &mut opus_frame)
            .map_err(|e| anyhow!("failed to encode Opus frame: {e:?}"))?;

        tx.send(make_packet(target, &opus_frame[..len]))
            .await
            .map_err(|_| anyhow!("TeamSpeak sender stopped"))?;
    }

    tx.send(make_packet(target, &[]))
        .await
        .map_err(|_| anyhow!("TeamSpeak sender stopped"))?;

    Ok(())
}

fn make_packet(target: &PlaybackTarget, data: &[u8]) -> OutPacket {
    let codec = CodecType::OpusVoice;
    match target {
        PlaybackTarget::CurrentChannel => OutAudio::new(&AudioData::C2S { id: 0, codec, data }),
        PlaybackTarget::ServerGroup { group_id, scope } => {
            OutAudio::new(&AudioData::C2SWhisperNew {
                id: 0,
                codec,
                whisper_type: 0,
                target: scope.wire_value(),
                target_id: *group_id,
                data,
            })
        }
    }
}
