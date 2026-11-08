use crate::AppWindow;
use slint::ComponentHandle;

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: slint::android::AndroidApp) {
    if let Err(error) = crate::with_async_runtime(|| run_android(app)) {
        tracing::error!("Failed to initialize async runtime: {}", error);
    }
}

fn run_android(app: slint::android::AndroidApp) {
    if let Err(error) = slint::android::init(app) {
        tracing::error!("Failed to initialize Slint Android backend: {}", error);
        return;
    }

    let window = match AppWindow::new() {
        Ok(window) => window,
        Err(error) => {
            tracing::error!("Failed to create Android app window: {}", error);
            return;
        }
    };
    crate::setup_app(&window);
    if let Err(error) = window.run() {
        tracing::error!("Android app exited with error: {}", error);
    }
}

/// Manifest declarations do not grant CAMERA/RECORD_AUDIO on Android 6+.
/// Wait asynchronously so Slint remains responsive while Android shows its dialog.
pub(crate) async fn ensure_permissions() -> Result<(), String> {
    if check_permissions(true).map_err(|e| e.to_string())? {
        return Ok(());
    }
    for _ in 0..120 {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        if check_permissions(false).map_err(|e| e.to_string())? {
            return Ok(());
        }
    }
    // Text interaction remains available when the user declines device access.
    tracing::warn!("Camera/microphone permission not granted; device input may be unavailable");
    Ok(())
}

fn check_permissions(request: bool) -> jni::errors::Result<bool> {
    use jni::objects::{JObject, JValue};
    let context = ndk_context::android_context();
    // SAFETY: ndk-context retains the VM and Activity for this NativeActivity.
    let vm = unsafe { jni::JavaVM::from_raw(context.vm().cast()) }?;
    let mut env = vm.attach_current_thread()?;
    let activity = unsafe { JObject::from_raw(context.context().cast()) };
    env.with_local_frame(16, |env| {
        let names = [
            "android.permission.CAMERA",
            "android.permission.RECORD_AUDIO",
        ];
        let mut missing = Vec::new();
        for name in names {
            let name = env.new_string(name)?;
            if env
                .call_method(
                    &activity,
                    "checkSelfPermission",
                    "(Ljava/lang/String;)I",
                    &[JValue::Object(name.as_ref())],
                )?
                .i()?
                != 0
            {
                missing.push(name);
            }
        }
        if missing.is_empty() {
            return Ok(true);
        }
        if request {
            let permissions =
                env.new_object_array(missing.len() as i32, "java/lang/String", JObject::null())?;
            for (index, permission) in missing.iter().enumerate() {
                env.set_object_array_element(&permissions, index as i32, permission)?;
            }
            env.call_method(
                &activity,
                "requestPermissions",
                "([Ljava/lang/String;I)V",
                &[JValue::Object(permissions.as_ref()), JValue::Int(1)],
            )?;
        }
        Ok(false)
    })
}
