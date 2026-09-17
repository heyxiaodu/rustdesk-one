//! 升级会话：把 [`super::iroh_upgrade`] 的状态机接到真实的 iroh 传输上。
//!
//! 由 feature `iroh-transport` 控制，默认关闭。
//!
//! # 分层
//!
//! ```text
//!   iroh_upgrade.rs          纯状态机（无依赖，13 个测试里占 7 个）
//!         ↑
//!   iroh_upgrade_session.rs  本文件：把状态机接到 iroh 传输 + 消息编解码
//!         ↑
//!   src/server/connection.rs / src/client.rs   连接层接线（P3）
//! ```
//!
//! 这样分层的好处是：**协议正确性可以在本机完全验证**，
//! 而连接层只剩「把 Action 翻译成 send / 换流」这种机械工作。
//!
//! # 会话生命周期
//!
//! ```text
//!   UpgradeSession::new(role, cfg)
//!        │
//!        ├─ start()            → 发起方：产出 Offer 消息
//!        ├─ handle(union)      → 收到升级消息 → 产出要回的消息
//!        ├─ poll_dial()        → 拨号结果（非阻塞，供 select! 使用）
//!        ├─ tick()             → 超时检查
//!        └─ take_ready()       → 取出已就绪的新流（此时才做加密状态迁移）
//! ```

use super::iroh_upgrade::{Action, Event, Role, State, Upgrade, UPGRADE_PROTOCOL_VERSION};
use super::iroh_transport::{self, IrohConfig};
use crate::message_proto::{message, IrohUpgradeAnswer, IrohUpgradeCommit, IrohUpgradeCommitAck, IrohUpgradeOffer, Message};
use crate::tcp::FramedStream;
use crate::ResultType;
use anyhow::anyhow;
use iroh::{Endpoint, EndpointAddr, EndpointId};
use tokio::sync::mpsc;

/// 一条升级消息的解析结果。
///
/// 抽出来是为了让「消息 → 事件」这一步可以脱离 iroh 单独测试。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IncomingUpgrade {
    Offer { endpoint_addr: String, version: u32 },
    Answer { accepted: bool, endpoint_addr: String, reason: String },
    Commit,
    CommitAck,
}

impl IncomingUpgrade {
    /// 从 `Message` 的 union 里识别升级消息；不是升级消息则返回 `None`。
    pub fn from_union(union: &message::Union) -> Option<Self> {
        match union {
            message::Union::IrohUpgradeOffer(o) => Some(Self::Offer {
                endpoint_addr: o.endpoint_addr.clone(),
                version: o.version,
            }),
            message::Union::IrohUpgradeAnswer(a) => Some(Self::Answer {
                accepted: a.accepted,
                endpoint_addr: a.endpoint_addr.clone(),
                reason: a.reason.clone(),
            }),
            message::Union::IrohUpgradeCommit(_) => Some(Self::Commit),
            message::Union::IrohUpgradeCommitAck(_) => Some(Self::CommitAck),
            _ => None,
        }
    }

    fn to_event(&self) -> Event<'_> {
        match self {
            Self::Offer { endpoint_addr, version } => Event::OfferReceived {
                endpoint_addr,
                version: *version,
            },
            Self::Answer { accepted, endpoint_addr, reason } => Event::AnswerReceived {
                accepted: *accepted,
                endpoint_addr,
                reason,
            },
            Self::Commit => Event::CommitReceived,
            Self::CommitAck => Event::CommitAckReceived,
        }
    }
}

/// 把状态机产出的动作翻译成要发出去的 `Message`。
///
/// 只有 `SendOffer` / `SendAnswer` 需要带自己地址，因此单独传 `local_addr`。
pub fn action_to_message(action: &Action, local_addr: &str) -> Option<Message> {
    match action {
        Action::SendOffer => {
            let mut m = Message::new();
            let mut o = IrohUpgradeOffer::new();
            o.endpoint_addr = local_addr.to_owned();
            o.version = UPGRADE_PROTOCOL_VERSION;
            m.set_iroh_upgrade_offer(o);
            Some(m)
        }
        Action::SendAnswer { accepted, reason } => {
            let mut m = Message::new();
            let mut a = IrohUpgradeAnswer::new();
            a.accepted = *accepted;
            a.reason = reason.clone();
            // 只有接受时才回带自己的地址
            a.endpoint_addr = if *accepted { local_addr.to_owned() } else { String::new() };
            m.set_iroh_upgrade_answer(a);
            Some(m)
        }
        Action::SendCommit => {
            let mut m = Message::new();
            m.set_iroh_upgrade_commit(IrohUpgradeCommit::new());
            Some(m)
        }
        Action::SendCommitAck => {
            let mut m = Message::new();
            m.set_iroh_upgrade_commit_ack(IrohUpgradeCommitAck::new());
            Some(m)
        }
        // 这几个不是「发一条消息」那么简单，由会话自己处理
        Action::DialIroh | Action::SwitchTransport | Action::Fallback => None,
    }
}

/// 一次升级会话。
pub struct UpgradeSession {
    upgrade: Upgrade,
    cfg: IrohConfig,
    /// 本端 iroh endpoint（懒启动，只有真的要升级时才建）
    endpoint: Option<Endpoint>,
    /// 对端 iroh 身份（用于校验拨号目标）
    peer_id: Option<EndpointId>,
    /// 拨号结果通道
    dial_rx: Option<mpsc::UnboundedReceiver<ResultType<(FramedStream, EndpointId)>>>,
    /// 已就绪、等待切换的新流
    ready: Option<FramedStream>,
    /// 是否已经产出过 SwitchTransport（防止重复取）
    switch_armed: bool,
}

impl UpgradeSession {
    pub fn new(role: Role, cfg: IrohConfig) -> Self {
        Self {
            // local_addr 先留空，等 endpoint 起来后再填
            upgrade: Upgrade::new(role, String::new()),
            cfg,
            endpoint: None,
            peer_id: None,
            dial_rx: None,
            ready: None,
            switch_armed: false,
        }
    }

    pub fn role(&self) -> Role {
        self.upgrade.role()
    }

    pub fn state(&self) -> &State {
        self.upgrade.state()
    }

    /// 是否已经可以换流（连接层据此调用 [`Self::take_ready`]）。
    pub fn switch_armed(&self) -> bool {
        self.switch_armed
    }

    /// 取出已就绪的新流，并把老流的加密状态迁移过去。
    ///
    /// **调用方必须把返回的流替换掉原来的 `Connection.stream`。**
    /// 加密状态在这里迁移，正是为了避免 nonce 复用（见 `crypto_handoff` 模块）。
    pub fn take_ready(&mut self, old: &FramedStream) -> Option<FramedStream> {
        if !self.switch_armed {
            return None;
        }
        let mut new_stream = self.ready.take()?;
        new_stream.adopt_crypto_from(old);
        self.switch_armed = false;
        Some(new_stream)
    }

    /// 启动 endpoint 并返回本端地址。
    ///
    /// 只有真的要升级时才调用（懒启动），避免给不用这个功能的用户增加开销。
    pub async fn ensure_endpoint(&mut self) -> ResultType<String> {
        if self.endpoint.is_none() {
            let ep = iroh_transport::new_endpoint_from_config(self.cfg.clone()).await?;
            self.endpoint = Some(ep);
        }
        let ep = self.endpoint.as_ref().ok_or_else(|| anyhow!("endpoint 启动失败"))?;
        // 真实部署时这里应该是可被对端访问的地址集合；
        // 具体如何序列化/发布由信令层负责，这里先返回 addr 的 JSON。
        let addr = ep.addr();
        Ok(serde_json::to_string(&addr)?)
    }

    /// 发起方：开始升级。
    pub async fn start(&mut self) -> ResultType<Vec<Message>> {
        let local = self.ensure_endpoint().await?;
        self.upgrade = Upgrade::new(Role::Initiator, local);
        let actions = self.upgrade.step(Event::Start);
        Ok(self.apply(actions).await)
    }

    /// 处理一条收到的升级消息。
    pub async fn handle(&mut self, incoming: &IncomingUpgrade) -> ResultType<Vec<Message>> {
        // 响应方在收到 Offer 前可能还没有 endpoint
        if matches!(incoming, IncomingUpgrade::Offer { .. }) && self.upgrade.role() == Role::Responder {
            let local = self.ensure_endpoint().await?;
            self.upgrade = Upgrade::new(Role::Responder, local);
        }
        let event = incoming.to_event();
        let actions = self.upgrade.step(event);
        Ok(self.apply(actions).await)
    }

    /// 超时检查（连接层定时驱动）。
    pub async fn tick(&mut self) -> ResultType<Vec<Message>> {
        if self.upgrade.state().is_terminal() {
            return Ok(Vec::new());
        }
        let actions = self.upgrade.step(Event::Timeout);
        Ok(self.apply(actions).await)
    }

    /// 拨号结果（非阻塞），供 `select!` 使用。
    ///
    /// 返回 `true` 表示本次真的拿到了连接。
    pub async fn poll_dial(&mut self) -> ResultType<bool> {
        let Some(rx) = self.dial_rx.as_mut() else {
            return Ok(false);
        };
        match rx.try_recv() {
            Ok(Ok((stream, peer))) => {
                self.ready = Some(stream);
                self.peer_id = Some(peer);
                self.dial_rx = None;
                let actions = self.upgrade.step(Event::IrohConnected);
                self.apply(actions).await;
                Ok(true)
            }
            Ok(Err(e)) => {
                self.dial_rx = None;
                let msg = e.to_string();
                let actions = self.upgrade.step(Event::IrohFailed(&msg));
                self.apply(actions).await;
                Ok(false)
            }
            Err(mpsc::error::TryRecvError::Empty) => Ok(false),
            Err(mpsc::error::TryRecvError::Disconnected) => {
                self.dial_rx = None;
                let actions = self.upgrade.step(Event::IrohFailed("拨号任务意外结束"));
                self.apply(actions).await;
                Ok(false)
            }
        }
    }

    /// 执行状态机产出的一批动作。
    async fn apply(&mut self, actions: Vec<Action>) -> Vec<Message> {
        let mut out = Vec::new();
        let local = self
            .endpoint
            .as_ref()
            .map(|e| e.addr())
            .map(|a| serde_json::to_string(&a).unwrap_or_default())
            .unwrap_or_default();
        for action in actions {
            match action {
                Action::DialIroh => {
                    if let Err(e) = self.spawn_dial().await {
                        log::warn!("iroh 拨号启动失败: {e}");
                    }
                }
                Action::SwitchTransport => {
                    // 只有双方都走到这一步才会真正换流；
                    // 若此时还没有就绪的流，说明协议被违反了，直接放弃。
                    if self.ready.is_some() {
                        self.switch_armed = true;
                    } else {
                        log::warn!("收到切换指令但 iroh 流尚未就绪，放弃升级");
                    }
                }
                Action::Fallback => {
                    log::info!("iroh 升级未成功，继续使用原通道");
                    self.dial_rx = None;
                    self.ready = None;
                    self.switch_armed = false;
                }
                other => {
                    if let Some(m) = action_to_message(&other, &local) {
                        out.push(m);
                    }
                }
            }
        }
        out
    }

    async fn spawn_dial(&mut self) -> ResultType<()> {
        let ep = self
            .endpoint
            .as_ref()
            .ok_or_else(|| anyhow!("未启动 endpoint"))?
            .clone();
        let peer_json = self
            .upgrade
            .peer_addr()
            .ok_or_else(|| anyhow!("还不知道对端 iroh 地址"))?
            .to_owned();
        let peer: EndpointAddr = serde_json::from_str(&peer_json)
            .map_err(|e| anyhow!("对端 iroh 地址解析失败: {e}"))?;
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let _ = tx.send(iroh_transport::connect_from(&ep, peer).await);
        });
        self.dial_rx = Some(rx);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_upgrade_messages_only() {
        let mut m = Message::new();
        m.set_iroh_upgrade_commit(IrohUpgradeCommit::new());
        let union = m.union.clone().expect("应有 union");
        assert_eq!(
            IncomingUpgrade::from_union(&union),
            Some(IncomingUpgrade::Commit)
        );

        // 普通业务消息不应被当成升级消息
        let plain = Message::new();
        if let Some(u) = plain.union.as_ref() {
            assert_eq!(IncomingUpgrade::from_union(u), None);
        }
    }

    #[test]
    fn offer_carries_version_and_addr() {
        let mut o = IrohUpgradeOffer::new();
        o.endpoint_addr = "addr-A".to_owned();
        o.version = 7;
        let mut m = Message::new();
        m.set_iroh_upgrade_offer(o);
        let union = m.union.clone().unwrap();
        assert_eq!(
            IncomingUpgrade::from_union(&union),
            Some(IncomingUpgrade::Offer {
                endpoint_addr: "addr-A".to_owned(),
                version: 7
            })
        );
    }

    #[test]
    fn action_to_message_maps_all_wire_actions() {
        let m = action_to_message(&Action::SendOffer, "addr-A").expect("SendOffer 应有消息");
        match m.union {
            Some(message::Union::IrohUpgradeOffer(o)) => {
                assert_eq!(o.endpoint_addr, "addr-A");
                assert_eq!(o.version, UPGRADE_PROTOCOL_VERSION);
            }
            other => panic!("期望 Offer，实际 {:?}", other),
        }

        // 拒绝时不应回带地址
        let m = action_to_message(
            &Action::SendAnswer { accepted: false, reason: "版本不符".to_owned() },
            "addr-B",
        )
        .unwrap();
        match m.union {
            Some(message::Union::IrohUpgradeAnswer(a)) => {
                assert!(!a.accepted);
                assert!(a.endpoint_addr.is_empty(), "拒绝时不该回带自己的地址");
                assert_eq!(a.reason, "版本不符");
            }
            other => panic!("期望 Answer，实际 {:?}", other),
        }

        // 接受时要回带地址
        let m = action_to_message(
            &Action::SendAnswer { accepted: true, reason: String::new() },
            "addr-B",
        )
        .unwrap();
        match m.union {
            Some(message::Union::IrohUpgradeAnswer(a)) => assert_eq!(a.endpoint_addr, "addr-B"),
            other => panic!("期望 Answer，实际 {:?}", other),
        }

        assert!(matches!(
            action_to_message(&Action::SendCommit, "").unwrap().union,
            Some(message::Union::IrohUpgradeCommit(_))
        ));
        assert!(matches!(
            action_to_message(&Action::SendCommitAck, "").unwrap().union,
            Some(message::Union::IrohUpgradeCommitAck(_))
        ));

        // 非「发消息」的动作不产生消息
        assert!(action_to_message(&Action::DialIroh, "").is_none());
        assert!(action_to_message(&Action::SwitchTransport, "").is_none());
        assert!(action_to_message(&Action::Fallback, "").is_none());
    }

    /// 没有就绪的流时，即使状态机说要切换也**不能**把 switch_armed 置起来。
    #[tokio::test]
    async fn switch_is_not_armed_without_a_ready_stream() {
        let mut s = UpgradeSession::new(Role::Responder, IrohConfig::default());
        // 手工把状态推进到 AwaitingCommit 需要 endpoint，这里直接验证守卫逻辑：
        assert!(!s.switch_armed());
        // 没有 ready 流时 take_ready 必须返回 None
        let (a, _b) = tokio::io::duplex(1024);
        let fake = FramedStream(
            tokio_util::codec::Framed::new(
                crate::tcp::DynTcpStream(Box::new(a)),
                crate::bytes_codec::BytesCodec::new(),
            ),
            "127.0.0.1:0".parse().unwrap(),
            None,
            0,
        );
        assert!(s.take_ready(&fake).is_none(), "未武装时不得换流");
    }

    /// 会话在终态时 tick 不应再产出任何东西。
    #[tokio::test]
    async fn tick_is_noop_after_terminal() {
        let mut s = UpgradeSession::new(Role::Initiator, IrohConfig::default());
        // 直接把它推到 Failed
        s.upgrade = Upgrade::new(Role::Initiator, "x".to_owned());
        s.upgrade.step(Event::Start);
        s.upgrade.step(Event::Timeout);
        assert!(s.state().is_terminal());
        assert!(s.tick().await.unwrap().is_empty());
    }
}
