pub async fn pick_image() -> Result<(Vec<u8>, String), String> {
    #[cfg(not(target_os = "android"))]
    {
        pick_image_desktop().await
    }
    #[cfg(target_os = "android")]
    {
        pick_image_android().await
    }
}

#[cfg(not(target_os = "android"))]
async fn pick_image_desktop() -> Result<(Vec<u8>, String), String> {
    let file = rfd::AsyncFileDialog::new()
        .add_filter("Images", &["png", "jpg", "jpeg", "webp"])
        .pick_file()
        .await
        .ok_or_else(|| "未选择图片".to_string())?;

    let path = file.path().to_path_buf();
    let bytes = tokio::fs::read(file.path())
        .await
        .map_err(|error| format!("读取图片失败: {error}"))?;

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("未知文件")
        .to_string();

    Ok((bytes, file_name))
}

#[cfg(target_os = "android")]
async fn pick_image_android() -> Result<(Vec<u8>, String), String> {
    // On Android, we use a JNI-based approach to launch the system image picker
    // This is a simplified version - in production, you'd need to handle the
    // activity result callback through the Android activity lifecycle

    let ctx = ndk_context::android_context();
    let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }
        .map_err(|error| format!("获取 JVM 失败: {error}"))?;
    let activity = unsafe { jni::objects::JObject::from_raw(ctx.context().cast()) };
    let mut env = vm
        .attach_current_thread()
        .map_err(|error| format!("附加线程失败: {error}"))?;

    // Create intent for image picking
    let intent_class = env
        .find_class("android/content/Intent")
        .map_err(|error| format!("找不到 Intent 类: {error}"))?;

    let action = env
        .get_static_field(&intent_class, "ACTION_GET_CONTENT", "Ljava/lang/String;")
        .map_err(|error| format!("获取 ACTION_GET_CONTENT 失败: {error}"))?;
    let action = action
        .l()
        .map_err(|error| format!("读取 action 失败: {error}"))?;

    let intent = env
        .new_object(
            &intent_class,
            "(Ljava/lang/String;)V",
            &[jni::objects::JValue::Object(&action)],
        )
        .map_err(|error| format!("创建 Intent 失败: {error}"))?;

    let mime_type = env
        .new_string("image/*")
        .map_err(|error| format!("创建字符串失败: {error}"))?;

    env.call_method(
        &intent,
        "setType",
        "(Ljava/lang/String;)Landroid/content/Intent;",
        &[jni::objects::JValue::from(&jni::objects::JObject::from(
            mime_type,
        ))],
    )
    .map_err(|error| format!("设置类型失败: {error}"))?;

    // Launch the picker with a request code
    env.call_method(
        &activity,
        "startActivityForResult",
        "(Landroid/content/Intent;I)V",
        &[
            jni::objects::JValue::Object(&intent),
            jni::objects::JValue::Int(1001),
        ],
    )
    .map_err(|error| format!("启动图片选择器失败: {error}"))?;

    // In a real implementation, we'd wait for the result via a channel
    // For now, return an error indicating the picker was launched
    Err("请在弹出的选择器中选择图片".to_string())
}
