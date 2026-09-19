//! 升级会话：把 [`super::iroh_upgrade`] 的状态机接到真实的 iroh 传输上。
//!
//! 由 feature `iroh-transport` 控制（仅在 iroh 版构建中编译）。
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
//!        └─ poll_accept()      → 响应方：轮询进来的 iroh 连接（非阻塞）
//!
//! 换流不在这里做：会话产出 `Order::Install(新流)` 交给连接层装成
//! `TransitionStream`，加密状态的搬迁在过渡层切换那一刻完成
//! （见 `iroh_transition::TransitionStream::switch_to_new`）。
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

/// 会话要求连接层执行的**有序**指令。
///
/// 为什么要有序：过渡层必须在**发送 Commit 之前**就装好，
/// 否则 Commit 窗口内的应用数据不会被暂存，会静默丢失。
/// 把这些顺序约束集中在会话里，连接层只需照单执行，
/// 也就不容易接错。
pub enum Order {
    /// 安装过渡层（把老流与新的 iroh 流交给 [`crate::iroh_transition::TransitionStream`]）。
    /// 连接层应在此之后用过渡层替换 `Connection.stream`。
    Install(FramedStream),
    /// 走老通道发送这条消息。
    Send(Message),
    /// 过渡层：Commit 已经发出去了（此后不得再往老通道写应用数据）。
    CommitSent,
    /// 过渡层：收到了 CommitAck（可以切到新通道并冲刷暂存数据）。
    CommitAckReceived,
    /// 过渡层：CommitAck 已经发出去了（响应方直接切换）。
    CommitAckSent,
    /// 过渡层：升级失败，把暂存数据倒回老通道。
    Abort,
}

impl std::fmt::Debug for Order {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Order::Install(_) => write!(f, "Install(<iroh 流>)"),
            Order::Send(_) => write!(f, "Send(<消息>)"),
            Order::CommitSent => write!(f, "CommitSent"),
            Order::CommitAckReceived => write!(f, "CommitAckReceived"),
            Order::CommitAckSent => write!(f, "CommitAckSent"),
            Order::Abort => write!(f, "Abort"),
        }
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
    /// 待连接层执行的有序指令
    orders: Vec<Order>,
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
            orders: Vec::new(),
        }
    }

    /// 取出待执行的有序指令。
    ///
    /// 连接层在每次驱动会话（`start` / `handle` / `poll_dial` / `poll_accept` / `tick`）
    /// 之后调用一次，照序执行即可。
    pub fn drain_orders(&mut self) -> Vec<Order> {
        std::mem::take(&mut self.orders)
    }

    pub fn role(&self) -> Role {
        self.upgrade.role()
    }

    pub fn state(&self) -> &State {
        self.upgrade.state()
    }

    /// 这个会话还需要被定时驱动吗？
    ///
    /// 连接层的主循环是 `select!`，升级会话靠一条 100ms 定时分支驱动。
    /// 会话一旦进入终态（成功切换 / 被拒绝 / 失败回退），那条分支就再没有
    /// 任何事可做 —— 如果不把它关掉，每个长连接都会永远以 10Hz 空转，
    /// 阻止 CPU 进入深度空闲。所以 `select!` 的条件守卫应该用这个函数。
    ///
    /// 注意：只有「会话已经存在」时才需要判断，会话为 `None` 时
    /// 根本不应该有定时分支（`Option::is_some_and` 天然满足这一点）。
    pub fn needs_tick(&self) -> bool {
        !self.upgrade.state().is_terminal()
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
    pub async fn start(&mut self) -> ResultType<Vec<Order>> {
        let local = self.ensure_endpoint().await?;
        self.upgrade = Upgrade::new(Role::Initiator, local);
        let actions = self.upgrade.step(Event::Start);
        self.apply(actions).await;
        Ok(self.drain_orders())
    }

    /// 处理一条收到的升级消息。
    pub async fn handle(&mut self, incoming: &IncomingUpgrade) -> ResultType<Vec<Order>> {
        // 响应方在收到 Offer 前可能还没有 endpoint
        if matches!(incoming, IncomingUpgrade::Offer { .. }) && self.upgrade.role() == Role::Responder {
            let local = self.ensure_endpoint().await?;
            self.upgrade = Upgrade::new(Role::Responder, local);
        }
        let event = incoming.to_event();
        let actions = self.upgrade.step(event);
        self.apply(actions).await;
        Ok(self.drain_orders())
    }

    /// 超时检查（连接层定时驱动）。
    pub async fn tick(&mut self) -> ResultType<Vec<Order>> {
        if self.upgrade.state().is_terminal() {
            return Ok(Vec::new());
        }
        let actions = self.upgrade.step(Event::Timeout);
        self.apply(actions).await;
        Ok(self.drain_orders())
    }

    /// 对端 iroh 身份（连接就绪后才有值）。
    ///
    /// 上层应把它与预期设备的 pk 比对（我们已让两者等同，
    /// 见 `iroh_transport::secret_key_from_rustdesk`）。
    pub fn peer_id(&self) -> Option<EndpointId> {
        self.peer_id
    }

    /// 响应方：**非阻塞**轮询是否有进来的 iroh 连接。
    ///
    /// 连接层在 `select!` 里调用；只有处于 `Connecting` 阶段才需要轮询。
    pub async fn poll_accept(&mut self) -> ResultType<bool> {
        if self.upgrade.role() != Role::Responder
            || self.upgrade.state() != &State::Connecting
        {
            return Ok(false);
        }
        let Some(ep) = self.endpoint.clone() else {
            return Ok(false);
        };
        // 5ms：足够短，不会拖慢 select! 循环；又给 accept 一点机会完成
        match iroh_transport::try_accept(&ep, 5).await? {
            Some((stream, peer)) => {
                self.ready = Some(stream);
                self.peer_id = Some(peer);
                // 同 poll_dial：先装过渡层，再推进状态机
                self.emit_install_if_ready();
                let actions = self.upgrade.step(Event::IrohConnected);
                self.apply(actions).await;
                Ok(true)
            }
            None => Ok(false),
        }
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
                // 顺序关键：**先装过渡层，再推进状态机**。
                // 状态机会产出 Send(Commit)，而 Commit 必须走过渡层出去，
                // 否则提交窗口内的应用数据不会被暂存，会静默丢失。
                self.emit_install_if_ready();
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

    /// 执行状态机产出的一批动作，把副作用翻译成有序指令。
    async fn apply(&mut self, actions: Vec<Action>) {
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
                Action::SendCommit => {
                    // 顺序很关键：先把 Commit 交给老通道，
                    // 再通知过渡层进入提交窗口。
                    if let Some(m) = action_to_message(&action, &local) {
                        self.orders.push(Order::Send(m));
                    }
                    self.orders.push(Order::CommitSent);
                }
                Action::SendCommitAck => {
                    if let Some(m) = action_to_message(&action, &local) {
                        self.orders.push(Order::Send(m));
                    }
                    self.orders.push(Order::CommitAckSent);
                }
                Action::SwitchTransport => {
                    // 这一步是「通知过渡层可以换流了」。
                    //
                    // 两个角色在过渡层里走的是**不同**的入口：
                    //   * 响应方：先发 CommitAck，发完就能切 —— 那一步已经在
                    //     `Action::SendCommitAck` 里翻译成了 `Order::CommitAckSent`，
                    //     所以这里对响应方无事可做。
                    //   * 发起方：必须等收到 CommitAck 才能切，
                    //     对应的入口是 `Order::CommitAckReceived`。
                    //
                    // 以前这里什么都没产出（只置了一个没人读的开关），
                    // 结果状态机显示 Switched、过渡层却一直停在老通道上 ——
                    // 升级"成功"了，数据还走老路。
                    if self.upgrade.role() == Role::Initiator {
                        self.orders.push(Order::CommitAckReceived);
                    }
                }
                Action::Fallback => {
                    log::info!("iroh 升级未成功，继续使用原通道");
                    self.dial_rx = None;
                    self.ready = None;
                    self.orders.push(Order::Abort);
                }
                other => {
                    if let Some(m) = action_to_message(&other, &local) {
                        self.orders.push(Order::Send(m));
                    }
                }
            }
        }
    }

    /// 把就绪的 iroh 流交给过渡层安装。
    ///
    /// **必须在发 Commit 之前调用**，见 [`Order::Install`] 的说明。
    fn emit_install_if_ready(&mut self) {
        if let Some(stream) = self.ready.take() {
            self.orders.push(Order::Install(stream));
        }
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

    /// 发起方收到 CommitAck 之后，**必须真的产出一条换流指令**。
    ///
    /// 这是端到端测试挖出来的第二个「看起来成功、实际没换」的缺陷：
    /// 状态机明明走到了 `Switched`，但 `Action::SwitchTransport` 当时
    /// 什么都不产出（只置了一个没人读的开关），于是过渡层一直停在老通道上
    /// —— 升级报成功，数据还走老路。单元测试当时测的是"没 ready 流时不武装"，
    /// 恰好绕过了真正要断言的这条路径。
    #[tokio::test]
    async fn initiator_emits_switch_order_on_commit_ack() {
        use crate::bytes_codec::BytesCodec;
        use crate::tcp::{DynTcpStream, FramedStream};
        use iroh::SecretKey;
        use tokio_util::codec::Framed;

        let mut s = UpgradeSession::new(Role::Initiator, IrohConfig::default());
        s.upgrade = Upgrade::new(Role::Initiator, "addr-A".to_owned());
        s.upgrade.step(Event::Start);
        s.upgrade.step(Event::AnswerReceived {
            accepted: true,
            endpoint_addr: "addr-B",
            reason: "",
        });
        assert_eq!(s.state(), &State::Connecting);

        // 拨号完成 -> [Install, Send(Commit), CommitSent]
        let (a, _b) = tokio::io::duplex(4096);
        let framed = FramedStream(
            Framed::new(DynTcpStream(Box::new(a)), BytesCodec::new()),
            "127.0.0.1:0".parse().unwrap(),
            None,
            0,
        );
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(Ok((framed, SecretKey::generate().public()))).ok();
        s.dial_rx = Some(rx);
        assert!(s.poll_dial().await.unwrap());
        let _ = s.drain_orders();

        // 收到 CommitAck -> 必须产出换流指令
        let orders = s
            .handle(&IncomingUpgrade::CommitAck)
            .await
            .expect("处理 CommitAck 失败");
        assert_eq!(s.state(), &State::Switched, "应当进入 Switched");
        assert_eq!(
            orders.len(),
            1,
            "收到 CommitAck 必须产出恰好一条换流指令，实际 {:?}", orders
        );
        assert!(
            matches!(orders[0], Order::CommitAckReceived),
            "换流指令应当是 CommitAckReceived，实际 {:?}",
            orders[0]
        );
    }

    /// 响应方的换流入口是「CommitAck 已发出」，不能是 CommitAckReceived。
    /// （响应方根本没有收到过 CommitAck。）
    #[tokio::test]
    async fn responder_switches_on_ack_sent_not_received() {
        use crate::bytes_codec::BytesCodec;
        use crate::tcp::{DynTcpStream, FramedStream};
        use tokio_util::codec::Framed;

        let mut s = UpgradeSession::new(Role::Responder, IrohConfig::default());
        s.upgrade = Upgrade::new(Role::Responder, "addr-B".to_owned());
        s.upgrade.step(Event::OfferReceived {
            endpoint_addr: "addr-A",
            version: UPGRADE_PROTOCOL_VERSION,
        });
        let (a, _b) = tokio::io::duplex(4096);
        let framed = FramedStream(
            Framed::new(DynTcpStream(Box::new(a)), BytesCodec::new()),
            "127.0.0.1:0".parse().unwrap(),
            None,
            0,
        );
        s.ready = Some(framed);
        s.emit_install_if_ready();
        s.upgrade.step(Event::IrohConnected);
        assert_eq!(s.state(), &State::AwaitingCommit);
        let _ = s.drain_orders();

        let orders = s
            .handle(&IncomingUpgrade::Commit)
            .await
            .expect("处理 Commit 失败");
        assert_eq!(s.state(), &State::Switched);
        assert!(
            orders.iter().any(|o| matches!(o, Order::CommitAckSent)),
            "响应方必须产出 CommitAckSent，实际 {:?}", orders
        );
        assert!(
            !orders.iter().any(|o| matches!(o, Order::CommitAckReceived)),
            "响应方不该产出 CommitAckReceived（它没收到过 CommitAck），实际 {:?}", orders
        );
    }

    /// 终态之后必须**主动关掉**定时分支，否则每个长连接恒定 10Hz 空转。
    #[test]
    fn needs_tick_stops_at_terminal() {
        // 刚建好、还没 start：仍待驱动
        let s = UpgradeSession::new(Role::Initiator, IrohConfig::default());
        assert!(s.needs_tick());

        // 发起方：已发 Offer，等 Answer —— 要处理超时，仍需驱动
        let mut s = UpgradeSession::new(Role::Initiator, IrohConfig::default());
        s.upgrade = Upgrade::new(Role::Initiator, "addr-A".to_owned());
        s.upgrade.step(Event::Start);
        assert_eq!(s.state(), &State::OfferSent);
        assert!(s.needs_tick(), "等 Answer 时仍需驱动（要处理超时）");

        // 对端明确拒绝 -> 终态，定时分支必须关掉
        s.upgrade.step(Event::AnswerReceived {
            accepted: false,
            endpoint_addr: "",
            reason: "版本不兼容",
        });
        assert!(matches!(s.state(), State::Rejected(_)), "应进入 Rejected 终态");
        assert!(!s.needs_tick(), "被拒绝后不应再空转");

        // 响应方：建连失败回退 -> 终态
        let mut s = UpgradeSession::new(Role::Responder, IrohConfig::default());
        s.upgrade = Upgrade::new(Role::Responder, "addr-A".to_owned());
        s.upgrade.step(Event::OfferReceived {
            endpoint_addr: "addr-B",
            version: UPGRADE_PROTOCOL_VERSION,
        });
        assert_eq!(s.state(), &State::Connecting);
        assert!(s.needs_tick(), "等 iroh 建连时仍需驱动");
        s.upgrade.step(Event::IrohFailed("boom"));
        assert!(matches!(s.state(), State::Failed(_)), "应进入 Failed 终态");
        assert!(!s.needs_tick(), "建连失败回退后不应再空转");

        // 成功切到 iroh -> 终态（这正是 10Hz 空转的修复点）
        let mut s = UpgradeSession::new(Role::Initiator, IrohConfig::default());
        s.upgrade = Upgrade::new(Role::Initiator, "addr-A".to_owned());
        s.upgrade.step(Event::Start);
        s.upgrade.step(Event::AnswerReceived {
            accepted: true,
            endpoint_addr: "addr-B",
            reason: "",
        });
        s.upgrade.step(Event::IrohConnected);
        assert_eq!(s.state(), &State::CommitSent);
        assert!(s.needs_tick(), "等 CommitAck 时仍需驱动");
        s.upgrade.step(Event::CommitAckReceived);
        assert!(s.state().is_switched());
        assert!(!s.needs_tick(), "切换完成后不应再空转");
    }

    /// **把指令顺序钉死**：发起方在 iroh 连接就绪后，
    /// 必须先是 `Install`（装过渡层），再 `Send(Commit)`，最后 `CommitSent`。
    ///
    /// 顺序写反的后果：Commit 走老通道而过渡层还没装，
    /// 提交窗口内的应用数据不会被暂存 —— 静默丢失。
    #[tokio::test]
    async fn install_must_come_before_commit() {
        use crate::bytes_codec::BytesCodec;
        use crate::tcp::{DynTcpStream, FramedStream};
        use iroh::SecretKey;
        use tokio_util::codec::Framed;

        let mut s = UpgradeSession::new(Role::Initiator, IrohConfig::default());
        s.upgrade = Upgrade::new(Role::Initiator, "addr-A".to_owned());
        s.upgrade.step(Event::Start);
        s.upgrade.step(Event::AnswerReceived {
            accepted: true,
            endpoint_addr: "addr-B",
            reason: "",
        });
        assert_eq!(s.state(), &State::Connecting);

        // 直接往拨号通道塞一个成功结果，模拟拨号完成
        let (a, _b) = tokio::io::duplex(4096);
        let framed = FramedStream(
            Framed::new(DynTcpStream(Box::new(a)), BytesCodec::new()),
            "127.0.0.1:0".parse().unwrap(),
            None,
            0,
        );
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(Ok((framed, SecretKey::generate().public()))).ok();
        s.dial_rx = Some(rx);

        assert!(s.poll_dial().await.unwrap());
        let orders = s.drain_orders();
        let labels: Vec<&str> = orders
            .iter()
            .map(|o| match o {
                Order::Install(_) => "Install",
                Order::Send(_) => "Send",
                Order::CommitSent => "CommitSent",
                Order::CommitAckReceived => "CommitAckReceived",
                Order::CommitAckSent => "CommitAckSent",
                Order::Abort => "Abort",
            })
            .collect();
        assert_eq!(
            labels,
            vec!["Install", "Send", "CommitSent"],
            "必须先安装过渡层，再发 Commit（顺序反了会静默丢数据）"
        );
    }

    /// 只有响应方、且处于 Connecting 阶段才需要轮询接受连接；
    /// 其余情况必须立刻返回 false，不能阻塞 select! 循环。
    #[tokio::test]
    async fn poll_accept_is_noop_for_initiator_and_wrong_phase() {
        // 发起方：永远不该轮询 accept
        let mut a = UpgradeSession::new(Role::Initiator, IrohConfig::default());
        assert!(!a.poll_accept().await.unwrap());

        // 响应方但还没收到 Offer（Idle 阶段）：也不该轮询
        let mut b = UpgradeSession::new(Role::Responder, IrohConfig::default());
        assert!(!b.poll_accept().await.unwrap());
        assert_eq!(b.state(), &State::Idle);
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
