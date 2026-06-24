use ale_gui::AppWindow;
use slint::ComponentHandle;

fn main() -> Result<(), slint::PlatformError> {
    let app = AppWindow::new()?;
    ale_gui::setup_app(&app);
    app.run()
}
