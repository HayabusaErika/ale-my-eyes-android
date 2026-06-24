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
    running: Arc<Mutex<bool>>,
    config: CameraConfig,
}

impl AndroidCamera {
    pub fn new(config: CameraConfig) -> Self {
        Self {
            latest_frame: Arc::new(Mutex::new(None)),
            running: Arc::new(Mutex::new(false)),
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
        let mut running = self
            .running
            .lock()
            .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to lock running flag: {}", e)))?;

        if *running {
            return Ok(());
        }
        *running = true;
        drop(running);

        let latest_frame = self.latest_frame.clone();
        let running = self.running.clone();
        let width = self.config.width;
        let height = self.config.height;

        // 在后台线程中初始化相机
        std::thread::spawn(move || {
            if let Err(e) = init_camera(latest_frame, running, width, height) {
                tracing::error!("Camera initialization failed: {}", e);
            }
        });

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
        if let Ok(mut running) = self.running.lock() {
            *running = false;
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
fn init_camera(
    latest_frame: Arc<Mutex<Option<CameraFrame>>>,
    running: Arc<Mutex<bool>>,
    width: u32,
    height: u32,
) -> Result<()> {
    use jni::objects::{JObject, JValue};

    let ctx = ndk_context::android_context();
    let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }
        .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to get JVM: {}", e)))?;
    let activity = unsafe { JObject::from_raw(ctx.context().cast()) };
    let mut env = vm
        .attach_current_thread()
        .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to attach thread: {}", e)))?;

    let camera_manager = {
        let name = env
            .new_string("camera")
            .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to create string: {}", e)))?;
        let jname = JObject::from(name);
        env.call_method(
            &activity,
            "getSystemService",
            "(Ljava/lang/String;)Ljava/lang/Object;",
            &[JValue::Object(&jname)],
        )
        .map_err(|e| AleError::Other(anyhow::anyhow!("getSystemService failed: {}", e)))?
        .l()
        .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to unwrap CameraManager: {}", e)))?
    };

    let camera_id_list = env
        .call_method(&camera_manager, "getCameraIdList", "()[Ljava/lang/String;", &[])
        .map_err(|e| AleError::Other(anyhow::anyhow!("getCameraIdList failed: {}", e)))?
        .l()
        .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to unwrap camera ID list: {}", e)))?;

    let count = env
        .call_method(&camera_id_list, "length", "()I", &[])
        .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to get list length: {}", e)))?
        .i()
        .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to unwrap length: {}", e)))?;

    if count == 0 {
        return Err(AleError::Other(anyhow::anyhow!("No cameras found")));
    }

    let camera_id = env
        .call_method(&camera_id_list, "get", "(I)Ljava/lang/Object;", &[JValue::Int(0)])
        .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to get camera ID: {}", e)))?
        .l()
        .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to unwrap camera ID: {}", e)))?;

    let camera_device_class = env
        .find_class("android/hardware/camera2/CameraDevice")
        .map_err(|e| AleError::Other(anyhow::anyhow!("CameraDevice class not found: {}", e)))?;

    let state_callback_class = env
        .find_class("android/hardware/camera2/CameraDevice$StateCallback")
        .map_err(|e| AleError::Other(anyhow::anyhow!("StateCallback class not found: {}", e)))?;

    let state_callback = env
        .new_object(&state_callback_class, "()V", &[])
        .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to create StateCallback: {}", e)))?;

    let latch_class = env
        .find_class("java/util/concurrent/CountDownLatch")
        .map_err(|e| AleError::Other(anyhow::anyhow!("CountDownLatch class not found: {}", e)))?;

    let latch = env
        .new_object(&latch_class, "(I)V", &[JValue::Int(1)])
        .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to create CountDownLatch: {}", e)))?;

    let handler_class = env
        .find_class("android/os/Handler")
        .map_err(|e| AleError::Other(anyhow::anyhow!("Handler class not found: {}", e)))?;

    let looper_class = env
        .find_class("android/os/Looper")
        .map_err(|e| AleError::Other(anyhow::anyhow!("Looper class not found: {}", e)))?;

    let main_looper = env
        .call_static_method(&looper_class, "getMainLooper", "()Landroid/os/Looper;", &[])
        .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to get main looper: {}", e)))?
        .l()
        .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to unwrap main looper: {}", e)))?;

    let handler = env
        .new_object(&handler_class, "(Landroid/os/Looper;)V", &[JValue::Object(main_looper)])
        .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to create Handler: {}", e)))?;

    let camera_device_ref: Arc<Mutex<Option<JObject>>> = Arc::new(Mutex::new(None));
    let camera_device_ref_clone = camera_device_ref.clone();

    env.call_method(
        &camera_manager,
        "openCamera",
        "(Ljava/lang/String;Landroid/hardware/camera2/CameraDevice$StateCallback;Landroid/os/Handler;)V",
        &[
            JValue::Object(camera_id),
            JValue::Object(state_callback),
            JValue::Object(handler),
        ],
    )
    .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to open camera: {}", e)))?;

    let timeout_ns = 5_000_000_000i64;
    let time_unit_class = env
        .find_class("java/util/concurrent/TimeUnit")
        .map_err(|e| AleError::Other(anyhow::anyhow!("TimeUnit class not found: {}", e)))?;
    let nanos_unit = env
        .get_static_field(&time_unit_class, "NANOSECONDS", "Ljava/util/concurrent/TimeUnit;")
        .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to get TimeUnit: {}", e)))?
        .l()
        .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to unwrap TimeUnit: {}", e)))?;

    let await_result = env
        .call_method(
            &latch,
            "await",
            "(JLjava/util/concurrent/TimeUnit;)Z",
            &[JValue::Long(timeout_ns), JValue::Object(nanos_unit)],
        )
        .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to await camera: {}", e)))?
        .z()
        .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to unwrap await result: {}", e)))?;

    if !await_result {
        return Err(AleError::Other(anyhow::anyhow!("Camera open timed out")));
    }

    let camera_device = {
        let guard = camera_device_ref_clone
            .lock()
            .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to lock camera device: {}", e)))?;
        guard.ok_or_else(|| AleError::Other(anyhow::anyhow!("Camera device not available")))?
    };

    let image_reader_class = env
        .find_class("android/media/ImageReader")
        .map_err(|e| AleError::Other(anyhow::anyhow!("ImageReader class not found: {}", e)))?;

    let image_format_yuv = 0x23;
    let image_reader = env
        .call_static_method(
            &image_reader_class,
            "newInstance",
            "(III)Landroid/media/ImageReader;",
            &[
                JValue::Int(width as i32),
                JValue::Int(height as i32),
                JValue::Int(image_format_yuv),
            ],
        )
        .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to create ImageReader: {}", e)))?
        .l()
        .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to unwrap ImageReader: {}", e)))?;

    let surface = env
        .call_method(&image_reader, "getSurface", "()Landroid/view/Surface;", &[])
        .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to get Surface: {}", e)))?
        .l()
        .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to unwrap Surface: {}", e)))?;

    let surfaces_array_class = env
        .find_class("java/util/ArrayList")
        .map_err(|e| AleError::Other(anyhow::anyhow!("ArrayList class not found: {}", e)))?;

    let surfaces_array = env
        .new_object(&surfaces_array_class, "()V", &[])
        .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to create ArrayList: {}", e)))?;

    env.call_method(&surfaces_array, "add", "(Ljava/lang/Object;)Z", &[JValue::Object(surface)])
        .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to add Surface: {}", e)))?;

    let session_state_callback_class = env
        .find_class("android/hardware/camera2/CameraCaptureSession$StateCallback")
        .map_err(|e| AleError::Other(anyhow::anyhow!("Session StateCallback class not found: {}", e)))?;

    let session_callback = env
        .new_object(&session_state_callback_class, "()V", &[])
        .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to create session callback: {}", e)))?;

    let session_latch = env
        .new_object(&latch_class, "(I)V", &[JValue::Int(1)])
        .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to create session latch: {}", e)))?;

    env.call_method(
        &camera_device,
        "createCaptureSession",
        "(Ljava/util/List;Landroid/hardware/camera2/CameraCaptureSession$StateCallback;Landroid/os/Handler;)V",
        &[
            JValue::Object(surfaces_array),
            JValue::Object(session_callback),
            JValue::Object(handler),
        ],
    )
    .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to create capture session: {}", e)))?;

    let await_result = env
        .call_method(
            &session_latch,
            "await",
            "(JLjava/util/concurrent/TimeUnit;)Z",
            &[JValue::Long(timeout_ns), JValue::Object(nanos_unit)],
        )
        .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to await session: {}", e)))?
        .z()
        .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to unwrap await result: {}", e)))?;

    if !await_result {
        return Err(AleError::Other(anyhow::anyhow!("Session creation timed out")));
    }

    let template_preview = 1;
    let request_builder = env
        .call_method(
            &camera_device,
            "createCaptureRequest",
            "(I)Landroid/hardware/camera2/CaptureRequest$Builder;",
            &[JValue::Int(template_preview)],
        )
        .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to create CaptureRequest.Builder: {}", e)))?
        .l()
        .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to unwrap CaptureRequest.Builder: {}", e)))?;

    env.call_method(&request_builder, "addTarget", "(Landroid/view/Surface;)V", &[JValue::Object(surface)])
        .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to add target: {}", e)))?;

    let capture_request = env
        .call_method(&request_builder, "build", "()Landroid/hardware/camera2/CaptureRequest;", &[])
        .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to build CaptureRequest: {}", e)))?
        .l()
        .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to unwrap CaptureRequest: {}", e)))?;

    let capture_callback_class = env
        .find_class("android/hardware/camera2/CameraCaptureSession$CaptureCallback")
        .map_err(|e| AleError::Other(anyhow::anyhow!("CaptureCallback class not found: {}", e)))?;
    let capture_callback = env
        .new_object(&capture_callback_class, "()V", &[])
        .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to create CaptureCallback: {}", e)))?;

    env.call_method(
        &session_callback,
        "setRepeatingRequest",
        "(Landroid/hardware/camera2/CaptureRequest;Landroid/hardware/camera2/CameraCaptureSession$CaptureCallback;Landroid/os/Handler;)I",
        &[
            JValue::Object(capture_request),
            JValue::Object(capture_callback),
            JValue::Object(handler),
        ],
    )
    .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to start repeating request: {}", e)))?;

    tracing::info!("Camera2 preview started");

    while {
        let Ok(r) = running.lock() else {
            tracing::warn!("Camera running flag lock poisoned");
            return Ok(());
        };
        *r
    } {
        let image = env
            .call_method(&image_reader, "acquireLatestImage", "()Landroid/media/Image;", &[])
            .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to acquire image: {}", e)))?
            .l();

        if image.is_null() {
            std::thread::sleep(std::time::Duration::from_millis(10));
            continue;
        }

        let planes = env
            .call_method(&image, "getPlanes", "()[Landroid/media/Image$Plane;", &[])
            .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to get planes: {}", e)))?
            .l()
            .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to unwrap planes: {}", e)))?;

        let planes_length = env
            .call_method(&planes, "length", "()I", &[])
            .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to get planes length: {}", e)))?
            .i()
            .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to unwrap planes length: {}", e)))?;

        if planes_length >= 3 {
            let extract_plane = |env: &mut jni::JNIEnv, planes: &JObject, index: i32| -> Result<(Vec<u8>, i32)> {
                let plane_obj = env
                    .call_method(planes, "get", "(I)Ljava/lang/Object;", &[JValue::Int(index)])
                    .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to get plane {}: {}", index, e)))?
                    .l()
                    .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to unwrap plane {}: {}", index, e)))?;

                let buffer = env
                    .call_method(&plane_obj, "getBuffer", "()Ljava/nio/ByteBuffer;", &[])
                    .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to get buffer for plane {}: {}", index, e)))?
                    .l()
                    .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to unwrap buffer for plane {}: {}", index, e)))?;

                let remaining = env
                    .call_method(&buffer, "remaining", "()I", &[])
                    .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to get remaining for plane {}: {}", index, e)))?
                    .i()
                    .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to unwrap remaining for plane {}: {}", index, e)))?;

                let row_stride = env
                    .call_method(&plane_obj, "getRowStride", "()I", &[])
                    .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to get row stride for plane {}: {}", index, e)))?
                    .i()
                    .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to unwrap row stride for plane {}: {}", index, e)))?;

                Ok((vec![0u8; remaining as usize], row_stride))
            };

            let (y_data, y_stride) = extract_plane(&mut env, &planes, 0)?;
            let (u_data, uv_stride) = extract_plane(&mut env, &planes, 1)?;
            let (v_data, _) = extract_plane(&mut env, &planes, 2)?;

            let rgba_data = yuv420_to_rgba(&y_data, &u_data, &v_data, width, height, y_stride as u32, uv_stride as u32);

            let frame = CameraFrame {
                width,
                height,
                rgba_data,
                is_placeholder: false,
            };

            if let Ok(mut latest) = latest_frame.lock() {
                *latest = Some(frame);
            }
        }

        env.call_method(&image, "close", "()V", &[])
            .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to close image: {}", e)))?;

        std::thread::sleep(std::time::Duration::from_millis(33));
    }

    env.call_method(&camera_device, "close", "()V", &[])
        .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to close camera: {}", e)))?;

    env.call_method(&image_reader, "close", "()V", &[])
        .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to close ImageReader: {}", e)))?;

    Ok(())
}

#[cfg(target_os = "android")]
fn generate_placeholder_frame(width: u32, height: u32, tick: u32) -> CameraFrame {
    let mut rgba_data = vec![0u8; (width * height * 4) as usize];
    let stripe = (tick % 255) as u8;

    for y in 0..height {
        for x in 0..width {
            let idx = ((y * width + x) * 4) as usize;
            let gradient_x = ((x * 255) / width.max(1)) as u8;
            let gradient_y = ((y * 255) / height.max(1)) as u8;
            let pulse = stripe.wrapping_add(((x + y) % 64) as u8);

            rgba_data[idx] = gradient_x.saturating_add(pulse / 4);
            rgba_data[idx + 1] = gradient_y.saturating_add(pulse / 5);
            rgba_data[idx + 2] = 96u8.saturating_add(pulse / 3);
            rgba_data[idx + 3] = 255;
        }
    }

    CameraFrame {
        width,
        height,
        rgba_data,
        is_placeholder: true,
    }
}

#[cfg(not(target_os = "android"))]
fn init_camera(
    _latest_frame: Arc<Mutex<Option<CameraFrame>>>,
    _running: Arc<Mutex<bool>>,
    _width: u32,
    _height: u32,
) -> Result<()> {
    Err(AleError::Other(anyhow::anyhow!(
        "Camera only available on Android"
    )))
}

/// YUV_420_888 到 RGBA 转换
pub fn yuv420_to_rgba(
    y_plane: &[u8],
    u_plane: &[u8],
    v_plane: &[u8],
    width: u32,
    height: u32,
    y_stride: u32,
    uv_stride: u32,
) -> Vec<u8> {
    let mut rgba = vec![0u8; (width * height * 4) as usize];

    for row in 0..height {
        for col in 0..width {
            let y_idx = (row * y_stride + col) as usize;
            let uv_row = row / 2;
            let uv_col = col / 2;
            let uv_idx = (uv_row * uv_stride + uv_col) as usize;

            let y = y_plane.get(y_idx).copied().unwrap_or(128) as f32;
            let u = u_plane.get(uv_idx).copied().unwrap_or(128) as f32 - 128.0;
            let v = v_plane.get(uv_idx).copied().unwrap_or(128) as f32 - 128.0;

            let r = (y + 1.402 * v).clamp(0.0, 255.0) as u8;
            let g = (y - 0.344 * u - 0.714 * v).clamp(0.0, 255.0) as u8;
            let b = (y + 1.772 * u).clamp(0.0, 255.0) as u8;

            let rgba_idx = ((row * width + col) * 4) as usize;
            rgba[rgba_idx] = r;
            rgba[rgba_idx + 1] = g;
            rgba[rgba_idx + 2] = b;
            rgba[rgba_idx + 3] = 255;
        }
    }

    rgba
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_yuv_to_rgba() {
        let width = 2;
        let height = 2;
        let y = vec![128, 128, 128, 128];
        let u = vec![128];
        let v = vec![128];

        let rgba = yuv420_to_rgba(&y, &u, &v, width, height, 2, 1);
        assert_eq!(rgba.len(), 16); // 2*2*4
                                    // 灰色像素 (Y=128, U=128, V=128) -> R≈128, G≈128, B≈128
        assert!((rgba[0] as i32 - 128).abs() < 5);
        assert!((rgba[1] as i32 - 128).abs() < 5);
        assert!((rgba[2] as i32 - 128).abs() < 5);
        assert_eq!(rgba[3], 255); // Alpha
    }
}
