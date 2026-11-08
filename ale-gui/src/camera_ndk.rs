//! Camera2 NDK capture avoids Java abstract callbacks and keeps image buffers alive
//! until all planes have been copied into the published RGBA frame.
use super::{yuv420_to_rgba, CameraFrame, YuvPlane};
use ale_core::{AleError, Result};
use ndk_sys::*;
use std::ffi::{c_void, CStr};
use std::ptr::{null_mut, NonNull};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

// ndk-sys supplies the declarations but does not link the camera library.
#[link(name = "camera2ndk")]
extern "C" {}

fn error(message: impl std::fmt::Display) -> AleError {
    AleError::Other(anyhow::anyhow!("Camera: {}", message))
}
fn camera_ok(status: camera_status_t) -> Result<()> {
    if status.0 == 0 {
        Ok(())
    } else {
        Err(error(format!("Camera2 status {}", status.0)))
    }
}
fn media_ok(status: media_status_t) -> Result<()> {
    if status.0 == 0 {
        Ok(())
    } else {
        Err(error(format!("ImageReader status {}", status.0)))
    }
}

unsafe extern "C" fn disconnected(_: *mut c_void, _: *mut ACameraDevice) {
    tracing::warn!("Camera disconnected");
}
unsafe extern "C" fn device_error(_: *mut c_void, _: *mut ACameraDevice, code: i32) {
    tracing::error!("Camera device error: {}", code);
}
unsafe extern "C" fn session_state(_: *mut c_void, _: *mut ACameraCaptureSession) {}

/// Owned exclusively by the capture thread. Null members allow cleanup on partial failure.
#[derive(Default)]
struct Camera {
    manager: *mut ACameraManager,
    ids: *mut ACameraIdList,
    metadata: *mut ACameraMetadata,
    device: *mut ACameraDevice,
    reader: *mut AImageReader,
    outputs: *mut ACaptureSessionOutputContainer,
    output: *mut ACaptureSessionOutput,
    target: *mut ACameraOutputTarget,
    request: *mut ACaptureRequest,
    session: *mut ACameraCaptureSession,
}
impl Drop for Camera {
    fn drop(&mut self) {
        // SAFETY: every non-null member is owned by this object. Stop/close the
        // producers before releasing their targets and the reader's native window.
        // Callbacks have null context and never reference these resources.
        unsafe {
            if !self.session.is_null() {
                ACameraCaptureSession_stopRepeating(self.session);
                ACameraCaptureSession_close(self.session);
            }
            if !self.device.is_null() {
                ACameraDevice_close(self.device);
            }
            if !self.request.is_null() {
                ACaptureRequest_free(self.request);
            }
            if !self.target.is_null() {
                ACameraOutputTarget_free(self.target);
            }
            if !self.outputs.is_null() {
                ACaptureSessionOutputContainer_free(self.outputs);
            }
            if !self.output.is_null() {
                ACaptureSessionOutput_free(self.output);
            }
            if !self.reader.is_null() {
                AImageReader_delete(self.reader);
            }
            if !self.metadata.is_null() {
                ACameraMetadata_free(self.metadata);
            }
            if !self.ids.is_null() {
                ACameraManager_deleteCameraIdList(self.ids);
            }
            if !self.manager.is_null() {
                ACameraManager_delete(self.manager);
            }
        }
    }
}
impl Camera {
    fn open(width: u32, height: u32) -> Result<Self> {
        let mut camera = Self::default();
        // SAFETY: NDK out-pointers refer to initialized fields; each call is checked
        // before its result is used. Borrowed IDs/metadata/window outlive the session.
        unsafe {
            camera.manager = ACameraManager_create();
            if camera.manager.is_null() {
                return Err(error("Cannot create camera manager"));
            }
            camera_ok(ACameraManager_getCameraIdList(
                camera.manager,
                &mut camera.ids,
            ))?;
            if camera.ids.is_null() || (*camera.ids).numCameras == 0 {
                return Err(error("No camera available"));
            }
            let id = *(*camera.ids).cameraIds;
            tracing::info!("Opening camera {:?}", CStr::from_ptr(id));
            camera_ok(ACameraManager_getCameraCharacteristics(
                camera.manager,
                id,
                &mut camera.metadata,
            ))?;
            let mut entry = std::mem::zeroed();
            camera_ok(ACameraMetadata_getConstEntry(
                camera.metadata,
                acamera_metadata_tag::ACAMERA_SCALER_AVAILABLE_STREAM_CONFIGURATIONS.0,
                &mut entry,
            ))?;
            let configs = std::slice::from_raw_parts(entry.data.i32_, entry.count as usize);
            let sizes: Vec<_> = configs
                .chunks_exact(4)
                .filter(|c| {
                    c[0] == AIMAGE_FORMATS::AIMAGE_FORMAT_YUV_420_888.0 as i32
                        && c[3] == 0
                        && c[1] > 0
                        && c[2] > 0
                })
                .map(|c| (c[1], c[2]))
                .collect();
            let (width, height) = sizes
                .iter()
                .copied()
                .min_by_key(|&(w, h)| {
                    (i64::from(w) - i64::from(width)).abs()
                        + (i64::from(h) - i64::from(height)).abs()
                })
                .ok_or_else(|| error("Camera has no YUV output sizes"))?;
            let mut callbacks = ACameraDevice_StateCallbacks {
                context: null_mut(),
                onDisconnected: Some(disconnected),
                onError: Some(device_error),
            };
            camera_ok(ACameraManager_openCamera(
                camera.manager,
                id,
                &mut callbacks,
                &mut camera.device,
            ))?;
            media_ok(AImageReader_new(
                width,
                height,
                AIMAGE_FORMATS::AIMAGE_FORMAT_YUV_420_888.0 as i32,
                3,
                &mut camera.reader,
            ))?;
            let mut window = null_mut();
            media_ok(AImageReader_getWindow(camera.reader, &mut window))?;
            camera_ok(ACaptureSessionOutputContainer_create(&mut camera.outputs))?;
            camera_ok(ACaptureSessionOutput_create(window, &mut camera.output))?;
            camera_ok(ACaptureSessionOutputContainer_add(
                camera.outputs,
                camera.output,
            ))?;
            camera_ok(ACameraOutputTarget_create(window, &mut camera.target))?;
            camera_ok(ACameraDevice_createCaptureRequest(
                camera.device,
                ACameraDevice_request_template::TEMPLATE_PREVIEW,
                &mut camera.request,
            ))?;
            camera_ok(ACaptureRequest_addTarget(camera.request, camera.target))?;
            let callbacks = ACameraCaptureSession_stateCallbacks {
                context: null_mut(),
                onClosed: Some(session_state),
                onReady: Some(session_state),
                onActive: Some(session_state),
            };
            camera_ok(ACameraDevice_createCaptureSession(
                camera.device,
                camera.outputs,
                &callbacks,
                &mut camera.session,
            ))?;
            camera_ok(ACameraCaptureSession_setRepeatingRequest(
                camera.session,
                null_mut(),
                1,
                &mut camera.request,
                null_mut(),
            ))?;
        }
        Ok(camera)
    }

    fn latest_frame(&self) -> Result<Option<CameraFrame>> {
        let mut image = null_mut();
        // SAFETY: the reader remains live throughout this call.
        let status = unsafe { AImageReader_acquireLatestImage(self.reader, &mut image) };
        if status == media_status_t::AMEDIA_IMGREADER_NO_BUFFER_AVAILABLE {
            return Ok(None);
        }
        media_ok(status)?;
        let image = Image(NonNull::new(image).ok_or_else(|| error("Null image"))?);
        image.frame().map(Some)
    }
}

struct Image(NonNull<AImage>);
impl Drop for Image {
    fn drop(&mut self) {
        // SAFETY: this is the unique acquired image, and all plane borrows have ended.
        unsafe { AImage_delete(self.0.as_ptr()) }
    }
}
impl Image {
    fn plane(&self, index: i32) -> Result<YuvPlane<'_>> {
        let (mut data, mut length, mut row, mut pixel) = (null_mut(), 0, 0, 0);
        // SAFETY: NDK owns the plane data until this Image is deleted. The returned
        // slice borrows self and cannot outlive that ownership.
        unsafe {
            media_ok(AImage_getPlaneData(
                self.0.as_ptr(),
                index,
                &mut data,
                &mut length,
            ))?;
            media_ok(AImage_getPlaneRowStride(self.0.as_ptr(), index, &mut row))?;
            media_ok(AImage_getPlanePixelStride(
                self.0.as_ptr(),
                index,
                &mut pixel,
            ))?;
            if data.is_null() || length <= 0 || row <= 0 || pixel <= 0 {
                return Err(error("Invalid image plane"));
            }
            Ok(YuvPlane {
                data: std::slice::from_raw_parts(data, length as usize),
                row_stride: row as usize,
                pixel_stride: pixel as usize,
            })
        }
    }
    fn frame(&self) -> Result<CameraFrame> {
        let (mut width, mut height) = (0, 0);
        // SAFETY: image is valid and the out-pointers are live local integers.
        unsafe {
            media_ok(AImage_getWidth(self.0.as_ptr(), &mut width))?;
            media_ok(AImage_getHeight(self.0.as_ptr(), &mut height))?;
        }
        if width <= 0 || height <= 0 {
            return Err(error("Invalid image dimensions"));
        }
        let rgba_data = yuv420_to_rgba(
            self.plane(0)?,
            self.plane(1)?,
            self.plane(2)?,
            width as u32,
            height as u32,
        )?;
        Ok(CameraFrame {
            width: width as u32,
            height: height as u32,
            rgba_data,
            is_placeholder: false,
        })
    }
}

pub(super) fn capture(
    latest: Arc<Mutex<Option<CameraFrame>>>,
    running: Arc<AtomicBool>,
    width: u32,
    height: u32,
) -> Result<()> {
    let camera = Camera::open(width, height)?;
    let mut last_frame = Instant::now();
    while running.load(Ordering::Acquire) {
        if let Some(frame) = camera.latest_frame()? {
            *latest.lock().map_err(|_| error("Frame lock poisoned"))? = Some(frame);
            last_frame = Instant::now();
        } else if last_frame.elapsed() > Duration::from_secs(10) {
            return Err(error("Timed out waiting for camera frames"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}
