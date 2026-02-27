/// Encode a single PCM i16 sample to mu-law (ITU-T G.711).
pub fn mulaw_encode_sample(sample: i16) -> u8 {
    const BIAS: i32 = 0x84;
    const CLIP: i32 = 32635;

    let sign: u8 = if sample < 0 { 0x80 } else { 0x00 };
    let mut magnitude = if sample < 0 {
        -(sample as i32)
    } else {
        sample as i32
    };

    if magnitude > CLIP {
        magnitude = CLIP;
    }
    magnitude += BIAS;

    let mut exp: u8 = 7;
    let mut mask: i32 = 0x4000;
    while exp > 0 && (magnitude & mask) == 0 {
        exp -= 1;
        mask >>= 1;
    }

    let mantissa = ((magnitude >> (exp as i32 + 3)) & 0x0F) as u8;
    !(sign | (exp << 4) | mantissa)
}

/// Decode a single mu-law byte to PCM i16.
pub fn mulaw_decode_sample(byte: u8) -> i16 {
    let byte = !byte;
    let sign = byte & 0x80;
    let exp = ((byte >> 4) & 0x07) as i32;
    let mantissa = (byte & 0x0F) as i32;

    let magnitude = ((mantissa << 1) | 0x21) << (exp + 2);
    let magnitude = magnitude - 0x84;

    if sign != 0 {
        -(magnitude as i16)
    } else {
        magnitude as i16
    }
}

/// Encode a PCM i16 buffer to mu-law bytes.
pub fn encode_mulaw(pcm: &[i16]) -> Vec<u8> {
    pcm.iter().map(|&s| mulaw_encode_sample(s)).collect()
}

/// Decode mu-law bytes to PCM i16 buffer.
pub fn decode_mulaw(mulaw: &[u8]) -> Vec<i16> {
    mulaw.iter().map(|&b| mulaw_decode_sample(b)).collect()
}

/// Downmix interleaved stereo i16 samples to mono (average L+R).
pub fn downmix_stereo(interleaved: &[i16]) -> Vec<i16> {
    interleaved
        .chunks_exact(2)
        .map(|pair| ((pair[0] as i32 + pair[1] as i32) / 2) as i16)
        .collect()
}

/// Linear interpolation resampling.
pub fn resample_linear(input: &[i16], from_rate: u32, to_rate: u32) -> Vec<i16> {
    if from_rate == to_rate || input.is_empty() {
        return input.to_vec();
    }

    let ratio = from_rate as f64 / to_rate as f64;
    let output_len = ((input.len() as f64) / ratio).ceil() as usize;
    let mut output = Vec::with_capacity(output_len);

    for i in 0..output_len {
        let src_pos = i as f64 * ratio;
        let src_idx = src_pos as usize;
        let frac = src_pos - src_idx as f64;

        let sample = if src_idx + 1 < input.len() {
            let a = input[src_idx] as f64;
            let b = input[src_idx + 1] as f64;
            (a + (b - a) * frac) as i16
        } else {
            input[input.len() - 1]
        };

        output.push(sample);
    }

    output
}

/// Convert mono i16 PCM to interleaved stereo f32 raw bytes.
///
/// Each mono sample is normalized to [-1.0, 1.0] and duplicated to both channels.
/// Output is raw little-endian f32 bytes suitable for songbird's RawAdapter.
pub fn mono_i16_to_stereo_f32_bytes(mono: &[i16]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(mono.len() * 2 * 4);
    for &sample in mono {
        let f = sample as f32 / 32768.0;
        let le = f.to_le_bytes();
        bytes.extend_from_slice(&le); // left
        bytes.extend_from_slice(&le); // right
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mulaw_roundtrip() {
        // Roundtrip should be close (mu-law is lossy but within ~1 LSB)
        for sample in [-32000i16, -1000, 0, 1000, 32000] {
            let encoded = mulaw_encode_sample(sample);
            let decoded = mulaw_decode_sample(encoded);
            let diff = (sample as i32 - decoded as i32).abs();
            assert!(
                diff < 1000,
                "sample={sample}, decoded={decoded}, diff={diff}"
            );
        }
    }

    #[test]
    fn resample_identity() {
        let input = vec![1, 2, 3, 4, 5];
        let output = resample_linear(&input, 8000, 8000);
        assert_eq!(input, output);
    }

    #[test]
    fn resample_downsample() {
        // 48kHz → 8kHz: 6:1 ratio, 960 samples → 160
        let input: Vec<i16> = (0..960).map(|i| (i * 10) as i16).collect();
        let output = resample_linear(&input, 48000, 8000);
        assert_eq!(output.len(), 160);
    }

    #[test]
    fn resample_upsample() {
        // 8kHz → 48kHz: 1:6 ratio, 160 samples → 960
        let input: Vec<i16> = (0..160).map(|i| (i * 100) as i16).collect();
        let output = resample_linear(&input, 8000, 48000);
        assert_eq!(output.len(), 960);
    }

    #[test]
    fn stereo_f32_byte_count() {
        let mono = vec![0i16; 960];
        let bytes = mono_i16_to_stereo_f32_bytes(&mono);
        // 960 mono → 1920 stereo f32 → 7680 bytes
        assert_eq!(bytes.len(), 960 * 2 * 4);
    }
}
