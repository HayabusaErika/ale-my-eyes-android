pub mod audio;
mod conversation;
pub mod file_picker;
pub mod tts_player;

#[cfg(target_os = "android")]
mod android;

#[cfg(target_os = "android")]
pub mod camera;

#[cfg(not(target_os = "android"))]
pub mod screen_capture;

#[cfg(not(target_os = "android"))]
pub mod automation;

use ale_core::actions::ActionPlan;
use ale_core::config::AppConfig;
use ale_core::vad::{VadState, VoiceActivityDetector};
use ale_core::{AleEngine, AleEngineFactory};
use conversation::handle_question_response;
use std::future::Future;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;

slint::include_modules!();

pub struct AppState {
    engine: Option<Arc<Mutex<AleEngine>>>,
    recorder: Option<audio::Recorder>,
    recording_started: Option<Instant>,
    vad_sample_offset: usize,
    auto_speak: bool,
    vad: VoiceActivityDetector,
    vad_active: bool,
    #[cfg(not(target_os = "android"))]
    screen_capture: Option<screen_capture::ScreenCapture>,
    #[cfg(not(target_os = "android"))]
    automation: Option<automation::AutomationEngine>,
    #[cfg(target_os = "android")]
    camera: Option<camera::AndroidCamera>,
    #[cfg(target_os = "android")]
    camera_preview_timer: Option<slint::Timer>,
    pending_plan: Option<ActionPlan>,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    pub fn new() -> Self {
        Self {
            engine: None,
            recorder: None,
            recording_started: None,
            vad_sample_offset: 0,
            auto_speak: true,
            vad: VoiceActivityDetector::with_default_config(),
            vad_active: false,
            #[cfg(not(target_os = "android"))]
            screen_capture: None,
            #[cfg(not(target_os = "android"))]
            automation: None,
            #[cfg(target_os = "android")]
            camera: None,
            #[cfg(target_os = "android")]
            camera_preview_timer: None,
            pending_plan: None,
        }
    }
}

pub fn setup_app(app: &AppWindow) {
    let state = Arc::new(Mutex::new(AppState::new()));
    let app_weak = app.as_weak();
    app.set_is_android(cfg!(target_os = "android"));

    // Initialize engine + start monitoring
    {
        let state = state.clone();
        let app_weak = app_weak.clone();
        spawn_local_task(async move {
            let result = create_engine().await;
            let mut st = state.lock().await;
            let Some(app) = app_weak.upgrade() else {
                return;
            };

            match result {
                Ok((engine, config)) => {
                    apply_config_to_app(&app, &config);
                    let config_path = ale_core::config::ConfigFactory::create_default()
                        .config_path()
                        .to_string_lossy()
                        .to_string();
                    app.set_config_path(config_path.into());

                    st.engine = Some(engine);
                    app.set_engine_ready(true);
                    app.set_status_text("就绪".into());
                    app.set_status_type("ready".into());

                    let camera_available = initialize_platform_services(&mut st);
                    app.set_camera_available(camera_available);

                    // Auto-start continuous listening
                    if engine_supports_speech(&st.engine) {
                        start_continuous_listening(&mut st, &app);
                    }
                }
                Err(error) => {
                    app.set_status_text(slint::format!("初始化失败: {}", error));
                    app.set_status_type("error".into());
                }
            }
        });
    }

    #[cfg(target_os = "android")]
    {
        let timer_state = state.clone();
        let store_state = state.clone();
        let app_weak = app_weak.clone();
        let camera_timer = slint::Timer::default();
        camera_timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(250),
            move || {
                let state = timer_state.clone();
                let app_weak = app_weak.clone();
                spawn_local_task(async move {
                    let frame = {
                        let st = state.lock().await;
                        st.camera.as_ref().and_then(|cam| cam.latest_frame())
                    };
                    let Some(frame) = frame else {
                        return;
                    };
                    let Some(app) = app_weak.upgrade() else {
                        return;
                    };
                    app.set_camera_frame(camera_frame_to_slint_image(frame));
                    app.set_camera_frame_ready(true);
                });
            },
        );
        spawn_local_task(async move {
            store_state.lock().await.camera_preview_timer = Some(camera_timer);
        });
    }

    // VAD timer — checks for speech end every 100ms
    {
        let state = state.clone();
        let app_weak = app_weak.clone();
        let vad_timer = slint::Timer::default();
        vad_timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(100),
            move || {
                let state = state.clone();
                let app_weak = app_weak.clone();
                spawn_local_task(async move {
                    let samples = {
                        let mut st = state.lock().await;
                        if !st.vad_active || st.recorder.is_none() {
                            return;
                        }

                        let mut vad_sample_offset = st.vad_sample_offset;
                        let samples = if let Some(ref recorder) = st.recorder {
                            recorder.samples_since(&mut vad_sample_offset)
                        } else {
                            return;
                        };
                        st.vad_sample_offset = vad_sample_offset;
                        samples
                    };
                    if samples.is_empty() {
                        return;
                    }

                    let pcm = ale_core::vad::pcm16_bytes_to_f32(&samples);
                    let Some(app) = app_weak.upgrade() else {
                        return;
                    };

                    let speech_ended = {
                        let mut st = state.lock().await;
                        if !st.vad_active || st.recorder.is_none() {
                            return;
                        }

                        let mut speech_ended = false;
                        for chunk in pcm.chunks(st.vad.config.frame_size) {
                            if chunk.len() == st.vad.config.frame_size {
                                let vad_state = st.vad.process_frame(chunk);
                                if vad_state == VadState::SpeechEnded {
                                    speech_ended = true;
                                }
                            }
                        }

                        match st.vad.state() {
                            VadState::Speaking => app.set_vad_state("speaking".into()),
                            VadState::SpeechEnded => app.set_vad_state("speech_ended".into()),
                            VadState::Silent => app.set_vad_state("silent".into()),
                        }

                        speech_ended
                    };

                    if !speech_ended {
                        return;
                    }

                    // Speech ended — stop recording and process
                    let (engine, recorder, auto_speak, image_data) = {
                        let mut st = state.lock().await;
                        let engine = st.engine.clone();
                        let recorder = st.recorder.take();
                        let auto_speak = st.auto_speak;
                        st.recording_started = None;
                        st.vad_active = false;
                        app.set_is_busy(true);
                        app.set_status_text("处理中...".into());
                        app.set_status_type("processing".into());

                        let image_data: Option<Vec<u8>> = {
                            #[cfg(not(target_os = "android"))]
                            {
                                st.screen_capture
                                    .as_ref()
                                    .and_then(|sc| sc.latest_frame_jpeg())
                            }
                            #[cfg(target_os = "android")]
                            {
                                st.camera.as_ref().and_then(|cam| cam.latest_frame_jpeg(80))
                            }
                        };

                        (engine, recorder, auto_speak, image_data)
                    };

                    let Some(engine) = engine else {
                        app.set_status_text("引擎未初始化".into());
                        app.set_status_type("error".into());
                        app.set_is_busy(false);
                        return;
                    };
                    let Some(recorder) = recorder else {
                        app.set_is_busy(false);
                        return;
                    };

                    let audio = match recorder.into_wav_bytes() {
                        Ok(a) => a,
                        Err(e) => {
                            app.set_status_text(slint::format!("录音失败: {}", e));
                            app.set_status_type("error".into());
                            app.set_is_busy(false);
                            return;
                        }
                    };

                    // Transcribe audio
                    let transcription = {
                        let eng = engine.lock().await;
                        eng.transcribe(&audio).await
                    };

                    let Some(app) = app_weak.upgrade() else {
                        return;
                    };

                    let question = match transcription {
                        Ok(ref text) => {
                            app.set_transcription(text.clone().into());
                            text.clone()
                        }
                        Err(ref e) => {
                            app.set_transcription(slint::format!("转写失败: {}", e));
                            app.set_is_busy(false);
                            app.set_status_text("就绪".into());
                            app.set_status_type("ready".into());
                            let mut st = state.lock().await;
                            start_continuous_listening(&mut st, &app);
                            return;
                        }
                    };

                    handle_question_response(
                        &state,
                        &app,
                        &app_weak,
                        engine.clone(),
                        question,
                        image_data,
                        auto_speak,
                    )
                    .await;

                    app.set_is_busy(false);

                    // Restart listening
                    let mut st = state.lock().await;
                    start_continuous_listening(&mut st, &app);
                });
            },
        );
    }

    // Text submitted
    {
        let state = state.clone();
        let app_weak = app_weak.clone();
        app.on_text_submitted(move |text| {
            let question: String = text.into();
            if question.is_empty() {
                return;
            }
            let state = state.clone();
            let app_weak = app_weak.clone();
            spawn_local_task(async move {
                let st = state.lock().await;
                let engine = st.engine.clone();
                let auto_speak = st.auto_speak;
                drop(st);

                let Some(engine) = engine else { return };

                let Some(app) = app_weak.upgrade() else {
                    return;
                };
                app.set_transcription(question.clone().into());
                app.set_is_busy(true);
                app.set_status_text("分析中...".into());
                app.set_status_type("processing".into());

                // Get the current visual context for the question.
                #[cfg(not(target_os = "android"))]
                let image_data = {
                    let st = state.lock().await;
                    st.screen_capture
                        .as_ref()
                        .and_then(|sc| sc.latest_frame_jpeg())
                };
                #[cfg(target_os = "android")]
                let image_data = {
                    let st = state.lock().await;
                    st.camera.as_ref().and_then(|cam| cam.latest_frame_jpeg(80))
                };

                handle_question_response(
                    &state,
                    &app,
                    &app_weak,
                    engine.clone(),
                    question,
                    image_data,
                    auto_speak,
                )
                .await;

                let Some(app) = app_weak.upgrade() else {
                    return;
                };
                app.set_is_busy(false);
            });
        });
    }

    // Confirm action
    {
        let state = state.clone();
        let app_weak = app_weak.clone();
        app.on_confirm_action(move || {
            let state = state.clone();
            let app_weak = app_weak.clone();
            spawn_local_task(async move {
                let mut st = state.lock().await;
                if let Some(plan) = st.pending_plan.take() {
                    #[cfg(not(target_os = "android"))]
                    {
                        if st.automation.is_none() {
                            let Some(app) = app_weak.upgrade() else {
                                return;
                            };
                            app.set_show_confirmation(false);
                            app.set_status_text("自动化引擎不可用".into());
                            app.set_status_type("error".into());
                            return;
                        }
                        drop(st);

                        let result = tokio::task::spawn_blocking(move || {
                            let mut ae = automation::AutomationEngine::new(
                                automation::AutomationConfig::default(),
                            )?;
                            ae.execute_plan(&plan)
                        })
                        .await
                        .map_err(|error| {
                            ale_core::AleError::Other(anyhow::anyhow!(
                                "Automation task failed: {}",
                                error
                            ))
                        })
                        .and_then(|result| result);

                        let Some(app) = app_weak.upgrade() else {
                            return;
                        };
                        app.set_show_confirmation(false);
                        match result {
                            Ok(result) => {
                                app.set_status_text(slint::format!(
                                    "执行完成: {} 步",
                                    result.actions_executed
                                ));
                                app.set_status_type("ready".into());
                            }
                            Err(e) => {
                                app.set_status_text(slint::format!("执行失败: {}", e));
                                app.set_status_type("error".into());
                            }
                        }
                    }
                    #[cfg(target_os = "android")]
                    {
                        let Some(app) = app_weak.upgrade() else {
                            return;
                        };
                        app.set_show_confirmation(false);
                        app.set_status_text(slint::format!(
                            "Android 暂不支持执行 {} 个桌面自动化动作",
                            plan.actions.len()
                        ));
                    }
                }
            });
        });
    }

    // Cancel action
    {
        let app_weak = app_weak.clone();
        app.on_cancel_action(move || {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            app.set_show_confirmation(false);
            app.set_confirmation_text("".into());
        });
    }

    // Open settings
    {
        let state = state.clone();
        let app_weak = app_weak.clone();
        app.on_open_settings(move || {
            let state = state.clone();
            let app_weak = app_weak.clone();
            spawn_local_task(async move {
                let st = state.lock().await;
                let Some(app) = app_weak.upgrade() else {
                    return;
                };
                if let Some(ref engine) = st.engine {
                    let eng = engine.lock().await;
                    apply_config_to_app(&app, eng.config());
                }
                app.set_show_settings(true);
            });
        });
    }

    // Close settings
    {
        let app_weak = app_weak.clone();
        app.on_close_settings(move || {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            app.set_show_settings(false);
        });
    }

    // Settings field callbacks
    {
        let app_weak = app_weak.clone();
        app.on_provider_changed(move |text| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            app.set_provider(text);
        });
    }
    {
        let app_weak = app_weak.clone();
        app.on_api_key_changed(move |text| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            app.set_api_key(text);
        });
    }
    {
        let app_weak = app_weak.clone();
        app.on_api_url_changed(move |text| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            app.set_api_url(text);
        });
    }
    {
        let app_weak = app_weak.clone();
        app.on_model_changed(move |text| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            app.set_model(text);
        });
    }
    {
        let app_weak = app_weak.clone();
        app.on_max_tokens_changed(move |text| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            app.set_max_tokens_str(text);
        });
    }
    {
        let state = state.clone();
        let app_weak = app_weak.clone();
        app.on_auto_speak_changed(move |value| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            app.set_auto_speak(value);
            let state = state.clone();
            spawn_local_task(async move {
                state.lock().await.auto_speak = value;
            });
        });
    }
    {
        let app_weak = app_weak.clone();
        app.on_toggle_api_key_visible(move || {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            app.set_show_api_key(!app.get_show_api_key());
        });
    }

    // Save settings
    {
        let state = state.clone();
        let app_weak = app_weak.clone();
        app.on_save_settings(move || {
            let state = state.clone();
            let app_weak = app_weak.clone();
            spawn_local_task(async move {
                let st = state.lock().await;
                let engine = st.engine.clone();
                drop(st);

                let Some(engine) = engine else { return };
                let Some(app) = app_weak.upgrade() else {
                    return;
                };
                let config = {
                    let engine = engine.lock().await;
                    config_from_app(&app, engine.config())
                };

                app.set_is_busy(true);

                let result = save_settings(engine, config).await;
                let Some(app) = app_weak.upgrade() else {
                    return;
                };
                let mut st = state.lock().await;

                match result {
                    Ok(new_config) => {
                        apply_config_to_app(&app, &new_config);
                        app.set_status_text("就绪".into());
                        app.set_status_type("ready".into());
                        app.set_show_settings(false);
                        if engine_supports_speech(&st.engine) {
                            start_continuous_listening(&mut st, &app);
                        } else {
                            stop_continuous_listening(&mut st);
                        }
                    }
                    Err(error) => {
                        app.set_status_text(slint::format!("保存失败: {}", error));
                        app.set_status_type("error".into());
                    }
                }
                app.set_is_busy(false);
            });
        });
    }

    // Test connection
    {
        let state = state.clone();
        let app_weak = app_weak.clone();
        app.on_test_connection(move || {
            let state = state.clone();
            let app_weak = app_weak.clone();
            spawn_local_task(async move {
                let st = state.lock().await;
                let engine = st.engine.clone();
                drop(st);

                let Some(engine) = engine else { return };
                let Some(app) = app_weak.upgrade() else {
                    return;
                };
                app.set_is_busy(true);

                let result = test_connection(engine).await;
                let Some(app) = app_weak.upgrade() else {
                    return;
                };

                match result {
                    Ok(true) => {
                        app.set_status_text("连接成功".into());
                        app.set_status_type("ready".into());
                    }
                    Ok(false) => {
                        app.set_status_text("连接失败".into());
                        app.set_status_type("error".into());
                    }
                    Err(e) => {
                        app.set_status_text(slint::format!("测试失败: {}", e));
                        app.set_status_type("error".into());
                    }
                }
                app.set_is_busy(false);
            });
        });
    }
}

fn spawn_local_task(future: impl Future<Output = ()> + 'static) {
    if let Err(error) = slint::spawn_local(future) {
        tracing::warn!("Failed to spawn UI task: {}", error);
    }
}

fn initialize_platform_services(st: &mut AppState) -> bool {
    #[cfg(not(target_os = "android"))]
    {
        let sc = screen_capture::ScreenCapture::new(screen_capture::CaptureConfig::default());
        if let Err(e) = sc.start() {
            tracing::warn!("Screen capture failed to start: {}", e);
        } else {
            st.screen_capture = Some(sc);
        }

        match automation::AutomationEngine::new(automation::AutomationConfig::default()) {
            Ok(ae) => st.automation = Some(ae),
            Err(e) => tracing::warn!("Automation engine failed: {}", e),
        }

        false
    }

    #[cfg(target_os = "android")]
    {
        let cam = camera::AndroidCamera::new(camera::CameraConfig::default());
        if let Err(e) = cam.start() {
            tracing::warn!("Camera failed to start: {}", e);
            false
        } else {
            st.camera = Some(cam);
            true
        }
    }
}

#[cfg(target_os = "android")]
fn camera_frame_to_slint_image(frame: camera::CameraFrame) -> slint::Image {
    rgba_frame_to_slint_image(frame.width, frame.height, &frame.rgba_data)
}

#[cfg(target_os = "android")]
fn rgba_frame_to_slint_image(width: u32, height: u32, rgba_data: &[u8]) -> slint::Image {
    let mut buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(width, height);
    for (pixel, rgba) in buffer
        .make_mut_slice()
        .iter_mut()
        .zip(rgba_data.chunks_exact(4))
    {
        *pixel = slint::Rgba8Pixel {
            r: rgba[0],
            g: rgba[1],
            b: rgba[2],
            a: rgba[3],
        };
    }
    slint::Image::from_rgba8(buffer)
}

fn engine_supports_speech(engine: &Option<Arc<Mutex<AleEngine>>>) -> bool {
    engine
        .as_ref()
        .and_then(|engine| engine.try_lock().ok())
        .map(|engine| engine.cloud_provider_supports_speech())
        .unwrap_or(false)
}

fn stop_continuous_listening(st: &mut AppState) {
    st.recorder = None;
    st.recording_started = None;
    st.vad_sample_offset = 0;
    st.vad_active = false;
    st.vad.reset();
}

fn start_continuous_listening(st: &mut AppState, app: &AppWindow) {
    if !engine_supports_speech(&st.engine) {
        stop_continuous_listening(st);
        app.set_vad_state("silent".into());
        app.set_status_text("当前云服务不支持语音输入，请使用文本输入".into());
        app.set_status_type("ready".into());
        return;
    }

    match audio::Recorder::start() {
        Ok(recorder) => {
            let sample_rate = recorder.sample_rate();
            let channels = recorder.channels();
            st.recorder = Some(recorder);
            st.recording_started = Some(Instant::now());
            st.vad_sample_offset = 0;
            st.vad.config.sample_rate = sample_rate;
            st.vad.config.frame_size = ((sample_rate as usize * channels as usize) / 50).max(1);
            st.vad.reset();
            st.vad_active = true;
            app.set_vad_state("silent".into());
        }
        Err(e) => {
            app.set_status_text(slint::format!("麦克风启动失败: {}", e));
            app.set_status_type("error".into());
        }
    }
}

fn apply_config_to_app(app: &AppWindow, config: &AppConfig) {
    app.set_provider(config.cloud_api.provider.clone().into());
    app.set_api_key(config.cloud_api.api_key.clone().into());
    app.set_api_url(config.cloud_api.api_url.clone().into());
    app.set_model(config.cloud_api.model.clone().into());
    app.set_max_tokens_str(config.cloud_api.max_tokens.to_string().into());
    app.set_auto_speak(config.ui.auto_speak);
}

fn config_from_app(app: &AppWindow, base: &AppConfig) -> AppConfig {
    let mut config = base.clone();
    config.cloud_api.provider = app.get_provider().to_string();
    config.cloud_api.api_key = app.get_api_key().to_string();
    config.cloud_api.api_url = app
        .get_api_url()
        .to_string()
        .trim_end_matches('/')
        .to_string();
    config.cloud_api.model = app.get_model().to_string();
    if let Ok(budget) = app.get_max_tokens_str().to_string().parse::<usize>() {
        config.cloud_api.max_tokens = budget;
    }
    config.ui.auto_speak = app.get_auto_speak();
    config
}

async fn create_engine() -> Result<(Arc<Mutex<AleEngine>>, AppConfig), String> {
    let engine = AleEngineFactory::create_default()
        .await
        .map_err(|error| error.to_string())?;
    let config = engine.config().clone();
    Ok((Arc::new(Mutex::new(engine)), config))
}

async fn save_settings(
    engine: Arc<Mutex<AleEngine>>,
    config: AppConfig,
) -> Result<AppConfig, String> {
    {
        let mut engine = engine.lock().await;
        engine
            .update_config(config)
            .map_err(|error| error.to_string())?;
        Ok(engine.config().clone())
    }
}

async fn test_connection(engine: Arc<Mutex<AleEngine>>) -> Result<bool, String> {
    let engine = engine.lock().await;
    ensure_api_key(engine.config())?;
    engine
        .test_cloud_api()
        .await
        .map_err(|error| error.to_string())
}

fn ensure_api_key(config: &AppConfig) -> Result<(), String> {
    if config.cloud_api.api_key.trim().is_empty() {
        return Err("API key 未配置".to_string());
    }
    Ok(())
}
