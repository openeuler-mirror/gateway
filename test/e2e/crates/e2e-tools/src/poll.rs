//! poll_until：等待 DB / HTTP 可观察效果的统一原语。红线：禁止固定 sleep。

use std::time::Duration;

/// 反复探测直到谓词返回 Some（携带证据值）；probe 报错视为"暂不满足"继续重试
/// （DB 抖动用例依赖此语义）。超时报错并附最后一次 probe 错误。
pub async fn poll_until<F, T>(timeout: Duration, step: Duration, mut probe: F) -> anyhow::Result<T>
where
    F: FnMut() -> futures::future::BoxFuture<'static, anyhow::Result<Option<T>>>,
    T: std::fmt::Debug,
{
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last_err: Option<String> = None;
    loop {
        match probe().await {
            Ok(Some(v)) => return Ok(v),
            Ok(None) => {}
            Err(e) => last_err = Some(e.to_string()),
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "poll timeout after {timeout:?} (last probe error: {last_err:?})"
            );
        }
        tokio::time::sleep(step).await;
    }
}
