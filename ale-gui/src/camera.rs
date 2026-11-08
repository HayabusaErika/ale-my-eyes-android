use ale_core::{AleError, Result};
use std::sync::{Arc, Mutex};

/// 相机帧数据
#[derive(Clone)]
pub struct CameraFrame {
    pub width: u32,
    pub height: u32,
    pub rgba_data: Vec<u8>,
    pub is_placeholder: bool,
}

/// 相机配置
#[derive(Debug, Clone)]
pub struct CameraConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl Default for CameraConfig {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            fps: 30,
        }
    }
}

/// Android 相机状态
pub struct AndroidCamera {
    latest_frame: Arc<Mutex<Option<CameraFrame>>>,
    running: Arc<std::sync::atomic::AtomicBool>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
    config: CameraConfig,
}

impl AndroidCamera {
    pub fn new(config: CameraConfig) -> Self {
        Self {
            latest_frame: Arc::new(Mutex::new(None)),
            running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            worker: Mutex::new(None),
            config,
        }
    }

    /// 打开相机并开始预览
    pub fn start(&self) -> Result<()> {
        #[cfg(not(target_os = "android"))]
        {
            return Err(AleError::Other(anyhow::anyhow!(
                "Camera only available on Android"
            )));
        }

        #[allow(unreachable_code)]
        self.start_impl()
    }

    #[allow(dead_code)]
    fn start_impl(&self) -> Result<()> {
        use std::sync::atomic::Ordering;
        let mut worker = self
            .worker
            .lock()
            .map_err(|_| AleError::Other(anyhow::anyhow!("Camera worker lock poisoned")))?;
        if self.running.load(Ordering::Acquire) {
            return Ok(());
        }
        if let Some(previous) = worker.take() {
            let _ = previous.join();
        }
        self.running.store(true, Ordering::Release);
        let latest_frame = self.latest_frame.clone();
        let running = self.running.clone();
        let width = self.config.width;
        let height = self.config.height;
        *worker = Some(std::thread::spawn(move || {
            let result = init_camera(latest_frame.clone(), running.clone(), width, height);
            running.store(false, Ordering::Release);
            if let Ok(mut latest) = latest_frame.lock() {
                *latest = None;
            }
            if let Err(error) = result {
                tracing::error!("Camera capture failed: {}", error);
            }
        }));
        Ok(())
    }

    /// 获取最新帧的 JPEG 数据（用于发送给 API）
    pub fn latest_frame_jpeg(&self, quality: u8) -> Option<Vec<u8>> {
        let frame = self.latest_frame()?;
        if frame.is_placeholder {
            return None;
        }
        frame_to_jpeg(&frame, quality).ok()
    }

    /// 停止相机
    pub fn stop(&self) {
        // Serialize stop/start so an old worker cannot be revived by a new start.
        if let Ok(mut worker) = self.worker.lock() {
            self.running
                .store(false, std::sync::atomic::Ordering::Release);
            if let Some(worker) = worker.take() {
                let _ = worker.join();
            }
        }
    }

    /// 获取最新帧
    pub fn latest_frame(&self) -> Option<CameraFrame> {
        self.latest_frame.lock().ok()?.clone()
    }

    /// 立即捕获一帧
    pub fn capture_frame(&self) -> Result<CameraFrame> {
        self.latest_frame()
            .ok_or_else(|| AleError::Other(anyhow::anyhow!("No camera frame available")))
    }
}

impl Drop for AndroidCamera {
    fn drop(&mut self) {
        self.stop();
    }
}

fn frame_to_jpeg(frame: &CameraFrame, quality: u8) -> Result<Vec<u8>> {
    let img = image::RgbaImage::from_raw(frame.width, frame.height, frame.rgba_data.clone())
        .ok_or_else(|| AleError::Other(anyhow::anyhow!("Failed to create image from frame")))?;
    let rgb_img = image::DynamicImage::ImageRgba8(img).to_rgb8();

    let mut buf = std::io::Cursor::new(Vec::new());
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality);
    rgb_img
        .write_with_encoder(encoder)
        .map_err(|e| AleError::Other(anyhow::anyhow!("JPEG encode failed: {}", e)))?;

    Ok(buf.into_inner())
}

#[cfg(target_os = "android")]
#[path = "camera_ndk.rs"]
mod native;

#[cfg(target_os = "android")]
fn init_camera(
    latest_frame: Arc<Mutex<Option<CameraFrame>>>,
    running: Arc<std::sync::atomic::AtomicBool>,
    width: u32,
    height: u32,
) -> Result<()> {
    native::capture(latest_frame, running, width, height)
}

#[cfg(not(target_os = "android"))]
fn init_camera(
    _latest_frame: Arc<Mutex<Option<CameraFrame>>>,
    _running: Arc<std::sync::atomic::AtomicBool>,
    _width: u32,
    _height: u32,
) -> Result<()> {
    Err(AleError::Other(anyhow::anyhow!(
        "Camera only available on Android"
    )))
}

/// A borrowed YUV plane. Strides are bytes, not pixel counts.
pub struct YuvPlane<'a> {
    pub data: &'a [u8],
    pub row_stride: usize,
    pub pixel_stride: usize,
}

impl YuvPlane<'_> {
    fn sample(&self, row: usize, col: usize) -> Result<u8> {
        let index = row
            .checked_mul(self.row_stride)
            .and_then(|base| {
                col.checked_mul(self.pixel_stride)
                    .and_then(|col| base.checked_add(col))
            })
            .ok_or_else(|| AleError::Other(anyhow::anyhow!("YUV plane index overflow")))?;
        self.data
            .get(index)
            .copied()
            .ok_or_else(|| AleError::Other(anyhow::anyhow!("Truncated YUV plane")))
    }
}

/// Convert YUV_420_888, including padded rows and interleaved chroma planes.
pub fn yuv420_to_rgba(
    y: YuvPlane<'_>,
    u: YuvPlane<'_>,
    v: YuvPlane<'_>,
    width: u32,
    height: u32,
) -> Result<Vec<u8>> {
    let size = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(|| AleError::Other(anyhow::anyhow!("Image size overflow")))?;
    if [
        y.row_stride,
        y.pixel_stride,
        u.row_stride,
        u.pixel_stride,
        v.row_stride,
        v.pixel_stride,
    ]
    .contains(&0)
    {
        return Err(AleError::Other(anyhow::anyhow!("Invalid YUV stride")));
    }
    let mut rgba = Vec::with_capacity(size);
    for row in 0..height as usize {
        for col in 0..width as usize {
            let y = y.sample(row, col)? as f32;
            let u = u.sample(row / 2, col / 2)? as f32 - 128.0;
            let v = v.sample(row / 2, col / 2)? as f32 - 128.0;
            rgba.extend_from_slice(&[
                (y + 1.402 * v).clamp(0.0, 255.0) as u8,
                (y - 0.344 * u - 0.714 * v).clamp(0.0, 255.0) as u8,
                (y + 1.772 * u).clamp(0.0, 255.0) as u8,
                255,
            ]);
        }
    }
    Ok(rgba)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn plane(data: &[u8], row_stride: usize, pixel_stride: usize) -> YuvPlane<'_> {
        YuvPlane {
            data,
            row_stride,
            pixel_stride,
        }
    }

    #[test]
    fn planar_gray_frame_preserves_luminance() {
        let rgba = yuv420_to_rgba(
            plane(&[128; 4], 2, 1),
            plane(&[128], 1, 1),
            plane(&[128], 1, 1),
            2,
            2,
        )
        .unwrap();
        assert_eq!(rgba, [128, 128, 128, 255].repeat(4));
    }

    #[test]
    fn interleaved_chroma_and_padded_rows_match_planar_image() {
        let expected = yuv420_to_rgba(
            plane(&[100; 8], 4, 1),
            plane(&[0, 255], 2, 1),
            plane(&[255, 0], 2, 1),
            4,
            2,
        )
        .unwrap();
        let actual = yuv420_to_rgba(
            plane(&[100, 100, 100, 100, 9, 9, 100, 100, 100, 100], 6, 1),
            plane(&[0, 9, 255], 4, 2),
            plane(&[255, 9, 0], 4, 2),
            4,
            2,
        )
        .unwrap();
        assert_eq!(actual, expected);
        assert_ne!(actual[0..4], actual[8..12]);
    }

    #[test]
    fn truncated_plane_returns_error() {
        assert!(yuv420_to_rgba(
            plane(&[128], 2, 1),
            plane(&[128], 1, 1),
            plane(&[128], 1, 1),
            2,
            2
        )
        .is_err());
    }
}
