use std::io::Write;

/// WAV 音频参数结构体
#[derive(Debug, Clone)]
pub struct WavConfig {
    pub sample_rate: u32,     // 采样率 (Hz)
    pub channels: u16,        // 声道数
    pub bits_per_sample: u16, // 位深度
}

impl Default for WavConfig {
    fn default() -> Self {
        Self {
            sample_rate: crate::audio::SAMPLE_RATE,
            channels: 1,         // 单声道
            bits_per_sample: 16, // 16-bit
        }
    }
}

pub fn create_unlimited_wav_header(config: &WavConfig) -> Vec<u8> {
    let mut wav_data = Vec::new();
    let mut cursor = std::io::Cursor::new(&mut wav_data);

    let bytes_per_sample = config.bits_per_sample / 8;
    let byte_rate = config.sample_rate * config.channels as u32 * bytes_per_sample as u32;
    let block_align = config.channels * bytes_per_sample;
    // let data_size = 0u32;
    // let file_size = 0u32;
    let data_size = 0xFFFFFFFFu32; // unknown data size
    let file_size = 0x7FFFFFFFu32;

    cursor.write_all(b"RIFF").unwrap(); // ChunkID
    cursor.write_all(&file_size.to_le_bytes()).unwrap(); // ChunkSize (little-endian)
    cursor.write_all(b"WAVE").unwrap(); // Format

    // fmt subchunk
    cursor.write_all(b"fmt ").unwrap(); // Subchunk1ID
    cursor.write_all(&16u32.to_le_bytes()).unwrap(); // Subchunk1Size (PCM = 16)
    cursor.write_all(&1u16.to_le_bytes()).unwrap(); // AudioFormat (PCM = 1)
    cursor.write_all(&config.channels.to_le_bytes()).unwrap(); // NumChannels
    cursor.write_all(&config.sample_rate.to_le_bytes()).unwrap(); // SampleRate
    cursor.write_all(&byte_rate.to_le_bytes()).unwrap(); // ByteRate
    cursor.write_all(&block_align.to_le_bytes()).unwrap(); // BlockAlign
    cursor
        .write_all(&config.bits_per_sample.to_le_bytes())
        .unwrap(); // BitsPerSample

    // data subchunk
    cursor.write_all(b"data").unwrap(); // Subchunk2ID
    cursor.write_all(&data_size.to_le_bytes()).unwrap(); // Subchunk2Size

    wav_data
}

/// 从 URL 里取 host(去 scheme/端口/路径);无 scheme 时直接对整串取 host 段。
pub fn url_host(url: &str) -> &str {
    url.split("://")
        .nth(1)
        .unwrap_or(url)
        .split([':', '/'])
        .next()
        .unwrap_or("")
}

/// 按字符数截断,超长以 ".." 结尾(结果 ≤ max_chars 个字符;中文按字算)。
pub fn truncate_ellipsis(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max_chars.saturating_sub(2)).collect();
    t.push_str("..");
    t
}
