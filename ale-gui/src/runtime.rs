/// Keep Tokio's workers alive and enter its context on the Slint event-loop thread.
/// Slint polls UI futures itself; network I/O, timers and blocking jobs still need Tokio.
pub fn with_async_runtime<T>(run: impl FnOnce() -> T) -> std::io::Result<T> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let _guard = runtime.enter();
    Ok(run())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_thread_has_a_live_runtime_for_io_and_blocking_jobs() {
        with_async_runtime(|| {
            let handle = tokio::runtime::Handle::current();
            let result = handle.block_on(async {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                tokio::task::spawn_blocking(|| 42).await.unwrap()
            });
            assert_eq!(result, 42);
        })
        .unwrap();
    }
}
