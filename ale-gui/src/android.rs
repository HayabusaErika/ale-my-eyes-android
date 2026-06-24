use crate::AppWindow;
use slint::ComponentHandle;

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: slint::android::AndroidApp) {
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
