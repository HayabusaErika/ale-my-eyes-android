use crate::{tts_player, AppState, AppWindow};
use ale_core::actions::{parse_action_plan_arguments, ActionPlan, RiskLevel};
use ale_core::cloud::ToolCall;
use ale_core::AleEngine;
use std::sync::Arc;
use tokio::sync::Mutex;

struct AssistantReply {
    content: String,
    tokens_used: usize,
    tool_calls: Option<Vec<ToolCall>>,
}

pub async fn handle_question_response(
    state: &Arc<Mutex<AppState>>,
    app: &AppWindow,
    app_weak: &slint::Weak<AppWindow>,
    engine: Arc<Mutex<AleEngine>>,
    question: String,
    image_data: Option<Vec<u8>>,
    auto_speak: bool,
) {
    let result = ask_question(app, engine.clone(), &question, image_data).await;

    match result {
        Ok(reply) => {
            app.set_ai_response(reply.content.clone().into());
            record_interaction(
                app,
                engine.clone(),
                &question,
                &reply.content,
                reply.tokens_used,
            )
            .await;

            if let Some(calls) = reply.tool_calls {
                apply_tool_calls(state, app, &calls).await;
            } else {
                state.lock().await.pending_plan = None;
                app.set_action_steps("".into());
                app.set_confirmation_text("".into());
                app.set_show_confirmation(false);
            }

            app.set_status_text("就绪".into());
            app.set_status_type("ready".into());

            if auto_speak && !reply.content.is_empty() {
                app.set_status_text("朗读中...".into());
                app.set_status_type("processing".into());
                if let Err(error) = speak_and_play(engine, &reply.content).await {
                    tracing::warn!("TTS playback failed: {}", error);
                }
                let Some(app) = app_weak.upgrade() else {
                    return;
                };
                app.set_status_text("就绪".into());
                app.set_status_type("ready".into());
            }
        }
        Err(error) => {
            app.set_ai_response(slint::format!("失败: {}", error));
            app.set_status_text("就绪".into());
            app.set_status_type("ready".into());
        }
    }
}

async fn ask_question(
    app: &AppWindow,
    engine: Arc<Mutex<AleEngine>>,
    question: &str,
    image_data: Option<Vec<u8>>,
) -> Result<AssistantReply, String> {
    if let Some(image_data) = image_data {
        app.set_status_text("分析画面...".into());
        let response = {
            let engine = engine.lock().await;
            engine
                .ask_about_image_with_tools(&image_data, question, automation_tools())
                .await
                .map_err(|error| error.to_string())?
        };

        return Ok(AssistantReply {
            content: response.content,
            tokens_used: response.tokens_used,
            tool_calls: response.tool_calls,
        });
    }

    app.set_status_text("思考中...".into());
    let response = {
        let engine = engine.lock().await;
        engine
            .ask_text(question)
            .await
            .map_err(|error| error.to_string())?
    };

    Ok(AssistantReply {
        content: response.content,
        tokens_used: response.tokens_used,
        tool_calls: None,
    })
}

async fn record_interaction(
    app: &AppWindow,
    engine: Arc<Mutex<AleEngine>>,
    question: &str,
    answer: &str,
    tokens_used: usize,
) {
    let mut engine = engine.lock().await;
    let ctx = engine.context_mut();
    ctx.add_user_message(question.to_string());
    ctx.add_assistant_message(answer.to_string());
    ctx.add_tokens(tokens_used);
    app.set_session_tokens(ctx.session_tokens() as i32);
    let _ = engine.learn_from_interaction(question, answer);
}

async fn apply_tool_calls(state: &Arc<Mutex<AppState>>, app: &AppWindow, calls: &[ToolCall]) {
    let mut descriptions = Vec::new();
    let mut pending_plan: Option<ActionPlan> = None;

    for call in calls {
        match parse_action_plan_arguments(&call.function.arguments) {
            Ok(plan) => {
                descriptions.extend(plan.describe_steps());
                merge_pending_plan(&mut pending_plan, plan);
            }
            Err(_) => descriptions.push(format!(
                "{}: {}",
                call.function.name, call.function.arguments
            )),
        }
    }

    app.set_action_steps(descriptions.join("\n").into());

    if let Some(plan) = pending_plan {
        let confirmation_text = plan.speak_text();
        state.lock().await.pending_plan = Some(plan);
        app.set_confirmation_text(confirmation_text.into());
        app.set_show_confirmation(true);
    } else {
        state.lock().await.pending_plan = None;
        app.set_confirmation_text("".into());
        app.set_show_confirmation(false);
    }
}

fn merge_pending_plan(pending_plan: &mut Option<ActionPlan>, plan: ActionPlan) {
    if let Some(existing) = pending_plan {
        if !plan.explanation.trim().is_empty() {
            if !existing.explanation.trim().is_empty() {
                existing.explanation.push('\n');
            }
            existing.explanation.push_str(&plan.explanation);
        }
        existing.risk_level = max_risk(existing.risk_level, plan.risk_level);
        existing.requires_confirmation |= plan.requires_confirmation;
        existing.actions.extend(plan.actions);
    } else {
        *pending_plan = Some(plan);
    }
}

fn max_risk(left: RiskLevel, right: RiskLevel) -> RiskLevel {
    if right > left {
        right
    } else {
        left
    }
}

fn automation_tools() -> Vec<serde_json::Value> {
    vec![serde_json::json!({
        "type": "function",
        "function": {
            "name": "execute_action_plan",
            "description": "Create a desktop automation plan for the visible screen. The app will show the plan to the user for confirmation before executing it.",
            "parameters": {
                "type": "object",
                "properties": {
                    "explanation": {
                        "type": "string",
                        "description": "Short natural-language explanation of what will be done."
                    },
                    "risk_level": {
                        "type": "string",
                        "enum": ["low", "medium", "high"]
                    },
                    "requires_confirmation": {
                        "type": "boolean",
                        "description": "Whether the user must confirm before execution. Use true for any action that changes data, opens apps, types, clicks, or uses files."
                    },
                    "actions": {
                        "type": "array",
                        "items": {
                            "oneOf": [
                                {
                                    "type": "object",
                                    "properties": {
                                        "type": { "type": "string", "enum": ["click"] },
                                        "x": { "type": "number" },
                                        "y": { "type": "number" },
                                        "button": { "type": "string", "enum": ["left", "right", "middle"] }
                                    },
                                    "required": ["type", "x", "y", "button"]
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "type": { "type": "string", "enum": ["double_click"] },
                                        "x": { "type": "number" },
                                        "y": { "type": "number" }
                                    },
                                    "required": ["type", "x", "y"]
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "type": { "type": "string", "enum": ["mouse_move"] },
                                        "x": { "type": "number" },
                                        "y": { "type": "number" }
                                    },
                                    "required": ["type", "x", "y"]
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "type": { "type": "string", "enum": ["scroll"] },
                                        "x": { "type": "number" },
                                        "y": { "type": "number" },
                                        "delta_x": { "type": "number" },
                                        "delta_y": { "type": "number" }
                                    },
                                    "required": ["type", "x", "y", "delta_x", "delta_y"]
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "type": { "type": "string", "enum": ["type"] },
                                        "text": { "type": "string" }
                                    },
                                    "required": ["type", "text"]
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "type": { "type": "string", "enum": ["key"] },
                                        "key": { "type": "string" },
                                        "modifiers": { "type": "array", "items": { "type": "string" } }
                                    },
                                    "required": ["type", "key", "modifiers"]
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "type": { "type": "string", "enum": ["wait"] },
                                        "ms": { "type": "integer" }
                                    },
                                    "required": ["type", "ms"]
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "type": { "type": "string", "enum": ["open_app"] },
                                        "name": { "type": "string" }
                                    },
                                    "required": ["type", "name"]
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "type": { "type": "string", "enum": ["open_url"] },
                                        "url": { "type": "string" }
                                    },
                                    "required": ["type", "url"]
                                }
                            ]
                        }
                    }
                },
                "required": ["explanation", "risk_level", "requires_confirmation", "actions"]
            }
        }
    })]
}

async fn speak_and_play(engine: Arc<Mutex<AleEngine>>, text: &str) -> Result<(), String> {
    let audio = {
        let engine = engine.lock().await;
        ensure_api_key(engine.config())?;
        if !engine.cloud_provider_supports_speech() {
            return Err("当前云服务不支持语音朗读".to_string());
        }
        engine
            .synthesize(text)
            .await
            .map_err(|error| error.to_string())?
    };

    tokio::task::spawn_blocking(move || tts_player::play_audio(&audio))
        .await
        .map_err(|error| format!("播放失败: {error}"))?
}

fn ensure_api_key(config: &ale_core::config::AppConfig) -> Result<(), String> {
    if config.cloud_api.api_key.trim().is_empty() {
        return Err("API key 未配置".to_string());
    }
    Ok(())
}
