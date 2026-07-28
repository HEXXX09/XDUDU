//! Provider 安全重试：只重试尚未获得有效响应的模型请求。

use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use tokio::sync::Mutex;

use super::{Provider, ProviderRequest, ProviderResponse, ProviderStreamSink};
use crate::error::{XduduError, XduduResult};

pub struct RetryingProvider {
    inner: Arc<dyn Provider>,
    max_attempts: u32,
    base_delay: Duration,
    max_delay: Duration,
    min_request_interval: Duration,
    last_request: Mutex<Option<tokio::time::Instant>>,
}

impl RetryingProvider {
    pub fn new(inner: Box<dyn Provider>, max_attempts: u32, base_delay: Duration) -> Self {
        Self::with_interval(inner, max_attempts, base_delay, Duration::ZERO)
    }

    pub fn with_interval(
        inner: Box<dyn Provider>,
        max_attempts: u32,
        base_delay: Duration,
        min_request_interval: Duration,
    ) -> Self {
        Self {
            inner: Arc::from(inner),
            max_attempts: max_attempts.max(1),
            base_delay,
            max_delay: Duration::from_secs(30),
            min_request_interval,
            last_request: Mutex::new(None),
        }
    }

    fn delay(&self, attempt: u32, error: &XduduError) -> Duration {
        if let Some(milliseconds) = error
            .details
            .get("retryAfterMs")
            .and_then(serde_json::Value::as_u64)
        {
            return Duration::from_millis(milliseconds).min(self.max_delay);
        }
        let exponent = attempt.saturating_sub(1).min(10);
        let base = self
            .base_delay
            .saturating_mul(2_u32.saturating_pow(exponent))
            .min(self.max_delay);
        let jitter_bound = (base.as_millis() as u64 / 2).max(1);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos() as u64;
        base.saturating_add(Duration::from_millis(nanos % jitter_bound))
            .min(self.max_delay)
    }

    async fn wait_or_cancel(request: &ProviderRequest, duration: Duration) -> XduduResult<()> {
        tokio::select! {
            _ = request.cancellation.cancelled() => {
                Err(XduduError::provider("Provider 重试等待已中断。", false))
            }
            _ = tokio::time::sleep(duration) => Ok(()),
        }
    }

    async fn wait_for_slot(&self, request: &ProviderRequest) -> XduduResult<()> {
        if self.min_request_interval.is_zero() {
            return Ok(());
        }
        let mut last = self.last_request.lock().await;
        if let Some(previous) = *last {
            let elapsed = previous.elapsed();
            if elapsed < self.min_request_interval {
                Self::wait_or_cancel(request, self.min_request_interval - elapsed).await?;
            }
        }
        *last = Some(tokio::time::Instant::now());
        Ok(())
    }
}

#[async_trait]
impl Provider for RetryingProvider {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    async fn chat(&self, request: ProviderRequest) -> XduduResult<ProviderResponse> {
        for attempt in 1..=self.max_attempts {
            self.wait_for_slot(&request).await?;
            match self.inner.chat(request.clone()).await {
                Ok(response) => return Ok(response),
                Err(error) if error.retryable && attempt < self.max_attempts => {
                    Self::wait_or_cancel(&request, self.delay(attempt, &error)).await?;
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("至少会执行一次 Provider 请求")
    }

    async fn stream_chat(
        &self,
        request: ProviderRequest,
        sink: &dyn ProviderStreamSink,
    ) -> XduduResult<ProviderResponse> {
        for attempt in 1..=self.max_attempts {
            self.wait_for_slot(&request).await?;
            match self.inner.stream_chat(request.clone(), sink).await {
                Ok(response) => return Ok(response),
                Err(error) if error.retryable && attempt < self.max_attempts => {
                    Self::wait_or_cancel(&request, self.delay(attempt, &error)).await?;
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("至少会执行一次 Provider 请求")
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use super::*;
    use crate::provider::{FinishReason, MessageRole, ProviderMessage, TokenUsage};

    struct FlakyProvider {
        responses: Mutex<VecDeque<XduduResult<ProviderResponse>>>,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Provider for FlakyProvider {
        fn name(&self) -> &'static str {
            "flaky"
        }

        async fn chat(&self, _request: ProviderRequest) -> XduduResult<ProviderResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.responses.lock().unwrap().pop_front().unwrap()
        }
    }

    fn request() -> ProviderRequest {
        ProviderRequest {
            session_id: "s".into(),
            model: "m".into(),
            messages: vec![],
            tools: vec![],
            system: String::new(),
            temperature: 0.0,
            max_output_tokens: 1,
            cancellation: tokio_util::sync::CancellationToken::new(),
        }
    }

    fn success() -> ProviderResponse {
        ProviderResponse {
            message: ProviderMessage::text(MessageRole::Assistant, "ok"),
            tool_calls: vec![],
            usage: TokenUsage::default(),
            finish_reason: FinishReason::Stop,
        }
    }

    #[tokio::test]
    async fn 只重试标记为可重试的错误() {
        let provider = FlakyProvider {
            responses: Mutex::new(VecDeque::from([
                Err(XduduError::provider("临时失败", true)),
                Ok(success()),
            ])),
            calls: AtomicUsize::new(0),
        };
        let retry = RetryingProvider::new(Box::new(provider), 3, Duration::from_millis(1));
        assert!(retry.chat(request()).await.is_ok());
    }

    #[tokio::test]
    async fn 不重试永久错误() {
        let provider = FlakyProvider {
            responses: Mutex::new(VecDeque::from([
                Err(XduduError::provider("永久失败", false)),
                Ok(success()),
            ])),
            calls: AtomicUsize::new(0),
        };
        let retry = RetryingProvider::new(Box::new(provider), 3, Duration::from_millis(1));
        assert!(retry.chat(request()).await.is_err());
    }

    #[tokio::test]
    async fn 请求间隔限制连续模型调用() {
        let provider = FlakyProvider {
            responses: Mutex::new(VecDeque::from([Ok(success()), Ok(success())])),
            calls: AtomicUsize::new(0),
        };
        let retry = RetryingProvider::with_interval(
            Box::new(provider),
            1,
            Duration::from_millis(1),
            Duration::from_millis(20),
        );
        retry.chat(request()).await.unwrap();
        let started = tokio::time::Instant::now();
        retry.chat(request()).await.unwrap();
        assert!(started.elapsed() >= Duration::from_millis(15));
    }
}
