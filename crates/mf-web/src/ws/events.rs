//! `mf-workflow.v1` events WS 语义(T7c,Issue #41;spec §5/§7.1)。
//!
//! resume(cursor) → kernel `subscribe_events` → hello(resume 回执)→
//! per-client 有界队列 fan-out(kernel ProjectionHub 提供;慢客户端
//! 只逐出自身)。epoch 旋转/gap/overflow 统一 `resync_required` +
//! close 4409;cursor 与 L-PUBLISH 同屏障。

use mf_kernel::kernel::{CoreKernel, KernelProblem};
use mf_kernel::projection::EventCursor;

use crate::problem::{close_code, Problem, ProblemCode, Retry};

/// WS 控制帧(web 层;transport 编解码属 #42)。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventsControl {
    /// 升级后首帧:cursor 恢复(stream_epoch + 字符串化 through_seq)。
    Resume {
        stream_epoch: String,
        through_seq: String,
    },
}

/// hello/resume 回执(u64 字符串化)。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EventsHelloWire {
    pub schema: &'static str,
    pub stream_epoch: String,
    pub first_available_seq: String,
    pub last_seq: String,
    /// resume 请求的 through_seq 超出 journal 保留窗口 → 客户端必须
    /// 重新拉全量 Snapshot。
    pub resync_required: bool,
}

/// 订阅会话(单 client;poll 驱动)。
pub struct EventsSession {
    subscription: mf_kernel::projection::EventSubscription,
    hello: EventsHelloWire,
    closed: Option<Problem>,
}

impl EventsSession {
    /// resume:cursor 交给 kernel(与 L-PUBLISH 同屏障判定 gap/epoch)。
    pub fn resume(kernel: &dyn CoreKernel, control: &EventsControl) -> Result<Self, Problem> {
        let EventsControl::Resume {
            stream_epoch,
            through_seq,
        } = control;
        let through_seq: u64 = through_seq.parse().map_err(|_| {
            Problem::new(
                ProblemCode::InvalidEnvelope,
                "through_seq 必须是 u64 字符串",
                Some(Retry::Never),
            )
        })?;
        let cursor = EventCursor {
            stream_epoch: mf_kernel::handles::StreamEpoch::parse(stream_epoch),
            through_seq,
        };
        let subscription = kernel.subscribe_events(cursor).map_err(kernel_problem)?;
        let hello = subscription_hello(&subscription, false);
        Ok(Self {
            subscription,
            hello,
            closed: None,
        })
    }

    pub fn hello(&self) -> &EventsHelloWire {
        &self.hello
    }

    /// 拉取本 client 队列(非阻塞;慢客户端/gap/epoch → 关闭会话并
    /// 指示 4409;不拖累其它 client——kernel 队列按 client 隔离)。
    pub fn poll(&mut self) -> PollOutcome {
        if let Some(problem) = &self.closed {
            return PollOutcome::Closed {
                close_code: close_code::RESYNC_OR_HISTORY_GAP,
                problem: problem.clone(),
            };
        }
        match self.subscription.poll() {
            Ok(events) => PollOutcome::Events(events),
            Err(problem) => {
                let code = match &problem {
                    KernelProblem::ResyncRequired => ProblemCode::ResyncRequired,
                    other => Problem::new(ProblemCode::InternalError, other.to_string(), None).code,
                };
                let web_problem = kernel_problem(problem);
                self.closed = Some(web_problem.clone());
                PollOutcome::Closed {
                    close_code: close_code::RESYNC_OR_HISTORY_GAP,
                    problem: web_problem,
                }
            }
        }
    }
}

fn subscription_hello(
    subscription: &mf_kernel::projection::EventSubscription,
    resync_required: bool,
) -> EventsHelloWire {
    // EventHello 的序列化形态即 wire(u64 十进制字符串);
    // stream_epoch/seq 由 subscription debug 之外的字段提供——经
    // kernel 公开 API 获取。
    let hello = subscription.hello();
    EventsHelloWire {
        schema: "mf-workflow-events.hello.v1",
        stream_epoch: hello.stream_epoch.as_str().to_string(),
        first_available_seq: hello.first_available_seq.to_string(),
        last_seq: hello.last_seq.to_string(),
        resync_required,
    }
}

fn kernel_problem(problem: KernelProblem) -> Problem {
    let code = match &problem {
        KernelProblem::ResyncRequired => ProblemCode::ResyncRequired,
        KernelProblem::ControllerLeaseExpired => ProblemCode::ControllerLeaseExpired,
        KernelProblem::ServiceUnavailable(_) => ProblemCode::ServiceUnavailable,
        _ => ProblemCode::InternalError,
    };
    Problem::new(code, problem.to_string(), Some(Retry::AfterResync))
}

/// poll 结果。
#[derive(Debug, Clone)]
pub enum PollOutcome {
    /// 事件批(kernel EventEnvelope;wire 投射=原样 serde)。
    Events(Vec<mf_kernel::projection::EventEnvelope>),
    /// 会话必须关闭(4409;慢客户端只影响自身)。
    Closed { close_code: u16, problem: Problem },
}

/// 命令速率限制(附录 A1:默认 40/s,burst=3×;每 client)。
pub struct CommandRateLimiter {
    rate_per_second: u32,
    burst: u32,
    tokens: f64,
    last_refill: std::time::Instant,
}

impl CommandRateLimiter {
    pub fn new(rate_per_second: u32) -> Self {
        let rate = rate_per_second.clamp(5, 200);
        Self {
            rate_per_second: rate,
            burst: rate * 3,
            tokens: (rate * 3) as f64,
            last_refill: std::time::Instant::now(),
        }
    }

    /// 令牌桶;超限 → 429 rate_limited。
    pub fn allow(&mut self) -> Result<(), Problem> {
        let elapsed = self.last_refill.elapsed().as_secs_f64();
        self.last_refill = std::time::Instant::now();
        self.tokens =
            (self.tokens + elapsed * f64::from(self.rate_per_second)).min(f64::from(self.burst));
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            Ok(())
        } else {
            Err(Problem::new(
                ProblemCode::RateLimited,
                format!("命令速率超限({}/s)", self.rate_per_second),
                Some(Retry::AfterRetryAfter),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limiter_burst_then_throttle() {
        let mut limiter = CommandRateLimiter::new(40);
        // burst 120 内全过
        for _ in 0..120 {
            limiter.allow().unwrap();
        }
        // 第 121 个立即 429
        assert_eq!(limiter.allow().unwrap_err().code, ProblemCode::RateLimited);
    }
}
