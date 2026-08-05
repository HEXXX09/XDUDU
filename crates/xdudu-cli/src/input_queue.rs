//! 共享终端输入路由。
//!
//! 全屏 TUI 会话期间只有一个生产者任务读取终端事件，按当前焦点把事件
//! 分发给唯一消费者（Composer、选择器或审批菜单），避免多个读取方争抢
//! 同一终端输入流。非 TUI 路径不使用本模块，仍走各自的阻塞读取。

use std::{
    collections::VecDeque,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use crossterm::event::Event;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

const MAX_PENDING_EVENTS: usize = 4096;

/// 输入焦点：同一时刻只允许一个消费者。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputFocus {
    /// 主循环的 Composer（空闲编辑与运行中排队输入共用）。
    Composer,
    /// 会话/模型/计划选择器。
    Picker,
    /// 上下文内审批菜单。
    Approval,
}

struct QueueState {
    focus: InputFocus,
    composer: VecDeque<Event>,
    picker: VecDeque<Event>,
    approval: VecDeque<Event>,
    closed: bool,
}

impl QueueState {
    fn queue_mut(&mut self, focus: InputFocus) -> &mut VecDeque<Event> {
        match focus {
            InputFocus::Composer => &mut self.composer,
            InputFocus::Picker => &mut self.picker,
            InputFocus::Approval => &mut self.approval,
        }
    }
}

/// 共享输入路由。事件生产者调用 [`InputRouter::produce`]，消费者调用
/// [`InputRouter::next_for`] 等待属于自己焦点的下一个事件。
pub(crate) struct InputRouter {
    state: Mutex<QueueState>,
    composer_notify: Notify,
    picker_notify: Notify,
    approval_notify: Notify,
    /// 是否已有生产者任务在运行（TUI 会话期间为 true）。
    active: AtomicBool,
    shutdown: CancellationToken,
}

/// 模态输入焦点守卫；即使绘制或读取失败，也会恢复进入前的焦点。
pub(crate) struct InputFocusGuard<'a> {
    router: &'a InputRouter,
    previous: InputFocus,
}

impl Drop for InputFocusGuard<'_> {
    fn drop(&mut self) {
        self.router.set_focus(self.previous);
    }
}

impl std::fmt::Debug for InputRouter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InputRouter")
            .finish_non_exhaustive()
    }
}

impl Default for InputRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl InputRouter {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(QueueState {
                focus: InputFocus::Composer,
                composer: VecDeque::new(),
                picker: VecDeque::new(),
                approval: VecDeque::new(),
                closed: false,
            }),
            composer_notify: Notify::new(),
            picker_notify: Notify::new(),
            approval_notify: Notify::new(),
            active: AtomicBool::new(false),
            shutdown: CancellationToken::new(),
        }
    }

    pub(crate) fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::SeqCst);
        if active {
            self.state.lock().unwrap().closed = false;
        } else {
            self.close();
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    pub(crate) fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    /// 切换焦点。模态焦点每次进入时清空自己的旧事件，避免上一次菜单
    /// 遗留的 Enter/方向键在新审批或选择器中生效。Composer 队列始终保留。
    pub(crate) fn set_focus(&self, focus: InputFocus) {
        let mut state = self.state.lock().unwrap();
        state.focus = focus;
        if focus != InputFocus::Composer {
            state.queue_mut(focus).clear();
        }
    }

    pub(crate) fn focus(&self) -> InputFocus {
        self.state.lock().unwrap().focus
    }

    pub(crate) fn acquire_focus(&self, focus: InputFocus) -> InputFocusGuard<'_> {
        let previous = self.focus();
        self.set_focus(focus);
        InputFocusGuard {
            router: self,
            previous,
        }
    }

    /// 生产者入口：把终端事件放入队列并唤醒一个消费者。
    pub(crate) fn produce(&self, event: Event) {
        let focus = {
            let mut state = self.state.lock().unwrap();
            if state.closed {
                return;
            }
            let focus = state.focus;
            let queue = state.queue_mut(focus);
            if queue.len() >= MAX_PENDING_EVENTS {
                queue.pop_front();
            }
            queue.push_back(event);
            focus
        };
        self.notify_for(focus).notify_one();
    }

    /// 消费者入口：等待并取走属于 `focus` 的下一个事件。
    ///
    /// 焦点不匹配时事件保持排队，由焦点持有者消费，不会出现双读。
    /// 锁只在表达式内短暂持有，不跨 await 点，保证 Future 可 Send。
    pub(crate) async fn next_for(&self, focus: InputFocus) -> Option<Event> {
        loop {
            let notified = self.notify_for(focus).notified();
            {
                let mut state = self.state.lock().unwrap();
                if state.closed {
                    return None;
                }
                if state.focus == focus
                    && let Some(event) = state.queue_mut(focus).pop_front()
                {
                    return Some(event);
                }
            }
            notified.await;
        }
    }

    /// 关闭输入源并唤醒所有等待者。终端读取失败和正常退出都必须调用。
    pub(crate) fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.composer_notify.notify_waiters();
        self.picker_notify.notify_waiters();
        self.approval_notify.notify_waiters();
    }

    fn notify_for(&self, focus: InputFocus) -> &Notify {
        match focus {
            InputFocus::Composer => &self.composer_notify,
            InputFocus::Picker => &self.picker_notify,
            InputFocus::Approval => &self.approval_notify,
        }
    }

    /// 非阻塞取走一个事件；焦点不匹配或队列为空时返回 None。
    #[cfg(test)]
    pub(crate) fn try_pop(&self, focus: InputFocus) -> Option<Event> {
        let mut state = self.state.lock().unwrap();
        if state.focus != focus {
            return None;
        }
        state.queue_mut(focus).pop_front()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[tokio::test]
    async fn 事件绑定产生时的焦点() {
        let router = InputRouter::new();
        router.produce(key(KeyCode::Char('a')));
        router.produce(key(KeyCode::Char('b')));

        // 焦点在 Composer 时，Approval 消费者不能取走事件。
        router.set_focus(InputFocus::Approval);
        assert!(router.try_pop(InputFocus::Composer).is_none());

        // 切换后的审批不得消费 Composer 中已排队的按键。
        router.produce(key(KeyCode::Char('y')));
        assert_eq!(
            router.next_for(InputFocus::Approval).await,
            Some(key(KeyCode::Char('y')))
        );

        // 切回 Composer 后，原输入仍按顺序保留。
        router.set_focus(InputFocus::Composer);
        assert_eq!(
            router.next_for(InputFocus::Composer).await,
            Some(key(KeyCode::Char('a')))
        );
        assert_eq!(
            router.next_for(InputFocus::Composer).await,
            Some(key(KeyCode::Char('b')))
        );
    }

    #[tokio::test]
    async fn 焦点切换唤醒等待中的消费者() {
        let router = Arc::new(InputRouter::new());
        let waiter = {
            let router = Arc::clone(&router);
            tokio::spawn(async move {
                // 焦点为 Picker 时 Composer 消费者保持等待。
                router.next_for(InputFocus::Composer).await
            })
        };
        router.set_focus(InputFocus::Picker);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());
        router.set_focus(InputFocus::Composer);
        router.produce(key(KeyCode::Char('x')));
        assert_eq!(waiter.await.unwrap(), Some(key(KeyCode::Char('x'))));
    }

    #[tokio::test]
    async fn 消费者按到达顺序取走事件() {
        let router = InputRouter::new();
        for code in [KeyCode::Char('1'), KeyCode::Char('2'), KeyCode::Char('3')] {
            router.produce(key(code));
        }
        let mut received = Vec::new();
        while let Some(event) = router.try_pop(InputFocus::Composer) {
            received.push(event);
        }
        assert_eq!(received.len(), 3);
        assert_eq!(received[0], key(KeyCode::Char('1')));
        assert_eq!(received[2], key(KeyCode::Char('3')));
    }

    #[tokio::test]
    async fn 关闭后等待者稳定返回_none() {
        let router = Arc::new(InputRouter::new());
        let waiter = {
            let router = Arc::clone(&router);
            tokio::spawn(async move { router.next_for(InputFocus::Composer).await })
        };
        router.close();
        assert_eq!(waiter.await.unwrap(), None);
    }
}
