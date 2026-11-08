use ale_gui::AppWindow;
use slint::ComponentHandle;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    ale_gui::with_async_runtime(|| -> Result<(), slint::PlatformError> {
        let app = AppWindow::new()?;
        ale_gui::setup_app(&app);
        app.run()
    })??;
    Ok(())
}
