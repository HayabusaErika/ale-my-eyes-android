use std::io::Cursor;
use std::sync::{Arc, Mutex as StdMutex};

#[cfg(target_os = "android")]
use oboe::{AudioInputCallback, AudioInputStreamSafe, AudioStream, DataCallbackResult, Mono};

const MAX_BUFFER_SECONDS: usize = 120;

#[derive(Debug, Default)]
struct SampleBuffer {
    samples: Vec<f32>,
    start_index: usize,
    next_index: usize,
}

impl SampleBuffer {
    fn push_samples<I>(&mut self, samples: I, sample_rate: usize, channels: usize)
    where
        I: IntoIterator<Item = f32>,
    {
        let old_len = self.samples.len();
        self.samples.extend(samples);
        self.next_index += self.samples.len().saturating_sub(old_len);

        let max_samples = sample_rate
            .saturating_mul(channels)
            .saturating_mul(MAX_BUFFER_SECONDS);
        if max_samples > 0 && self.samples.len() > max_samples {
            let drain_count = self.samples.len() - max_samples;
            self.samples.drain(..drain_count);
            self.start_index += drain_count;
        }
    }

    fn samples_since(&self, offset: &mut usize) -> Vec<f32> {
        if *offset < self.start_index {
            *offset = self.start_index;
        }
        if *offset > self.next_index {
            *offset = self.next_index;
        }

        let relative_offset = offset.saturating_sub(self.start_index);
        let samples = self.samples[relative_offset..].to_vec();
        *offset = self.next_index;
        samples
    }
}

pub struct Recorder {
    #[cfg(not(target_os = "android"))]
    stream: cpal::Stream,
    #[cfg(target_os = "android")]
    stream: Option<AudioStream<RecorderCallback>>,
    samples: Arc<StdMutex<SampleBuffer>>,
    sample_rate: u32,
    channels: u16,
}

impl Recorder {
    pub fn start() -> Result<Self, String> {
        #[cfg(not(target_os = "android"))]
        {
            Self::start_desktop()
        }
        #[cfg(target_os = "android")]
        {
            Self::start_android()
        }
    }

    #[cfg_attr(not(target_os = "android"), allow(unused_mut))]
    pub fn into_wav_bytes(mut self) -> Result<Vec<u8>, String> {
        let samples = self
            .samples
            .lock()
            .map_err(|_| "读取录音缓存失败".to_string())?
            .samples
            .clone();

        if samples.is_empty() {
            return Err("没有录到音频".to_string());
        }

        #[cfg(not(target_os = "android"))]
        {
            drop(self.stream);
        }

        #[cfg(target_os = "android")]
        if let Some(mut stream) = self.stream.take() {
            let _ = stream.stop();
        }

        let spec = hound::WavSpec {
            channels: self.channels,
            sample_rate: self.sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };

        let mut cursor = Cursor::new(Vec::new());
        {
            let mut writer = hound::WavWriter::new(&mut cursor, spec)
                .map_err(|error| format!("创建 WAV 失败: {error}"))?;
            for sample in samples {
                let sample = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                writer
                    .write_sample(sample)
                    .map_err(|error| format!("写入 WAV 失败: {error}"))?;
            }
            writer
                .finalize()
                .map_err(|error| format!("完成 WAV 失败: {error}"))?;
        }

        Ok(cursor.into_inner())
    }

    /// Return samples appended after `offset` as i16 PCM bytes without draining the recording.
    pub fn samples_since(&self, offset: &mut usize) -> Vec<u8> {
        let Ok(buffer) = self.samples.lock() else {
            return Vec::new();
        };
        let samples = buffer.samples_since(offset);

        let mut pcm = Vec::with_capacity(samples.len() * 2);
        for s in samples {
            let i = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
            pcm.extend_from_slice(&i.to_le_bytes());
        }
        pcm
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    #[cfg(not(target_os = "android"))]
    fn start_desktop() -> Result<Self, String> {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or_else(|| "没有找到可用麦克风".to_string())?;
        let supported_config = device
            .default_input_config()
            .map_err(|error| format!("获取麦克风配置失败: {error}"))?;
        let sample_format = supported_config.sample_format();
        let config: cpal::StreamConfig = supported_config.into();
        let sample_rate = config.sample_rate.0;
        let channels = config.channels;
        let samples = Arc::new(StdMutex::new(SampleBuffer::default()));

        let stream = match sample_format {
            cpal::SampleFormat::F32 => build_input_stream::<f32>(&device, &config, samples.clone()),
            cpal::SampleFormat::I16 => build_input_stream::<i16>(&device, &config, samples.clone()),
            cpal::SampleFormat::U16 => build_input_stream::<u16>(&device, &config, samples.clone()),
            other => Err(format!("不支持的麦克风采样格式: {other:?}")),
        }?;

        stream
            .play()
            .map_err(|error| format!("启动录音失败: {error}"))?;

        Ok(Self {
            stream,
            samples,
            sample_rate,
            channels,
        })
    }

    #[cfg(target_os = "android")]
    fn start_android() -> Result<Self, String> {
        use oboe::{AudioStreamBuilder, PerformanceMode, SharingMode};

        let samples = Arc::new(StdMutex::new(SampleBuffer::default()));
        let samples_clone = samples.clone();

        let mut stream = AudioStreamBuilder::default()
            .set_input()
            .set_performance_mode(PerformanceMode::LowLatency)
            .set_sharing_mode(SharingMode::Shared)
            .set_format::<f32>()
            .set_channel_count::<Mono>()
            .set_callback(RecorderCallback {
                samples: samples_clone,
            })
            .open_stream()
            .map_err(|error| format!("打开音频流失败: {error:?}"))?;

        stream
            .start()
            .map_err(|error| format!("启动录音失败: {error:?}"))?;

        let sample_rate = stream.get_sample_rate() as u32;

        Ok(Self {
            stream: Some(stream),
            samples,
            sample_rate,
            channels: 1,
        })
    }
}

#[cfg(target_os = "android")]
impl Drop for Recorder {
    fn drop(&mut self) {
        if let Some(mut stream) = self.stream.take() {
            let _ = stream.stop();
        }
    }
}

#[cfg(target_os = "android")]
struct RecorderCallback {
    samples: Arc<StdMutex<SampleBuffer>>,
}

#[cfg(target_os = "android")]
impl AudioInputCallback for RecorderCallback {
    type FrameType = (f32, Mono);

    fn on_audio_ready(
        &mut self,
        _stream: &mut dyn AudioInputStreamSafe,
        frames: &[f32],
    ) -> DataCallbackResult {
        if let Ok(mut buffer) = self.samples.lock() {
            buffer.push_samples(frames.iter().copied(), 48_000, 1);
        }
        DataCallbackResult::Continue
    }
}

#[cfg(not(target_os = "android"))]
fn build_input_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    samples: Arc<StdMutex<SampleBuffer>>,
) -> Result<cpal::Stream, String>
where
    T: cpal::Sample + cpal::SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    use cpal::traits::DeviceTrait;
    let sample_rate = config.sample_rate.0 as usize;
    let channels = config.channels as usize;

    device
        .build_input_stream(
            config,
            move |data: &[T], _| {
                if let Ok(mut buffer) = samples.lock() {
                    buffer.push_samples(
                        data.iter().copied().map(f32::from_sample),
                        sample_rate,
                        channels,
                    );
                }
            },
            move |error| {
                eprintln!("录音流错误: {error}");
            },
            None,
        )
        .map_err(|error| format!("创建录音流失败: {error}"))
}

trait FromSample<T> {
    fn from_sample(sample: T) -> f32;
}

impl FromSample<f32> for f32 {
    fn from_sample(sample: f32) -> f32 {
        sample
    }
}

impl FromSample<i16> for f32 {
    fn from_sample(sample: i16) -> f32 {
        sample as f32 / i16::MAX as f32
    }
}

impl FromSample<u16> for f32 {
    fn from_sample(sample: u16) -> f32 {
        (sample as f32 / u16::MAX as f32) * 2.0 - 1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_buffer_returns_new_samples_after_trim() {
        let mut buffer = SampleBuffer::default();
        let mut offset = 0;

        buffer.push_samples([1.0, 2.0, 3.0, 4.0], 1, 1);
        assert_eq!(buffer.samples_since(&mut offset), vec![1.0, 2.0, 3.0, 4.0]);

        buffer.push_samples([5.0, 6.0], 1, 1);
        assert_eq!(buffer.samples, vec![3.0, 4.0, 5.0, 6.0]);
        assert_eq!(buffer.samples_since(&mut offset), vec![5.0, 6.0]);
    }

    #[test]
    fn sample_buffer_clamps_stale_offset_after_trim() {
        let mut buffer = SampleBuffer::default();
        let mut offset = 0;

        buffer.push_samples([1.0, 2.0, 3.0, 4.0, 5.0], 1, 1);

        assert_eq!(buffer.samples, vec![2.0, 3.0, 4.0, 5.0]);
        assert_eq!(buffer.samples_since(&mut offset), vec![2.0, 3.0, 4.0, 5.0]);
    }
}
