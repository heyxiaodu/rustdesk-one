//! iroh 传输升级的状态机。
//!
//! 由 feature `iroh-transport` 控制，默认关闭。
//!
//! # 协议
//!
//! 目标是在**不改动 hbbs/hbbr** 的前提下，把已经建好的连接从老通道
//! （TCP / KCP / 中继）切到 iroh(QUIC)。做法是**先用老通道建连，
//! 再在连接中途升级**。
//!
//! 关键在于**切换时机的同步**。本协议利用老通道的一个性质：
//! 它是**有序可靠**的，所以「收到 Commit」就意味着老通道上先前的数据
//! 都已被按序处理完 —— 这个点就是天然的安全切换点。
//!
//! ```text
//!   发起方 A                                    响应方 B
//!      |                                           |
//!      |--- Offer(endpoint_addr_A, version) ------>|   (老通道)
//!      |<-- Answer(accepted, endpoint_addr_B) -----|   (老通道)
//!      |                                           |
//!      |=========== A 主动拨号建立 iroh ===========>|
//!      |                                           |
//!      |--- Commit ------------------------------>|   (老通道)
//!      |<-- CommitAck -----------------------------|   (老通道)
//!      |                                           |
//!      X======== 双方同时切到 iroh 通道 ==========X
//! ```
//!
//! 只有**发起方拨号**（响应方只接受），避免双方同时连接产生两条 QUIC 连接。
//!
//! # 与标准 RustDesk 的兼容性
//!
//! 标准客户端会按 proto3 规则忽略 33~36 号字段，因此不会报错，
//! 只是永远不回 Answer。发起方在 `OfferSent` 阶段超时后走
//! [`State::Failed`] 并回退到老通道即可，对用户表现为「升级没发生」。
//!
//! # 切换时必须做的事
//!
//! 状态机只产出 [`Action::SwitchTransport`]，**真正的切换由调用方完成**。
//! 调用方在那一步必须：
//!
//! 1. 用 [`crate::crypto_handoff::FramedStream::adopt_crypto_from`]
//!    把老流的加密状态（密钥 + nonce 收发计数）迁移到新流上 ——
//!    否则会重复使用 nonce，见该模块的说明；
//! 2. 保留老通道一段时间作为保底，而不是立刻 drop。

use crate::ResultType;
use anyhow::anyhow;

/// 升级协议版本。不兼容变更时递增，用于干净地拒绝。
pub const UPGRADE_PROTOCOL_VERSION: u32 = 1;

/// 本端在这次升级中的角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// 主动发起升级的一方（负责拨号建立 iroh 连接）
    Initiator,
    /// 被动接受的一方（只接受 iroh 连接）
    Responder,
}

/// 状态机的状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// 还没开始
    Idle,
    /// 发起方：已发出 Offer，等 Answer
    OfferSent,
    /// 双方：地址已交换，等 iroh 连接就绪
    Connecting,
    /// 响应方：iroh 已就绪，等对方发 Commit
    AwaitingCommit,
    /// 发起方：已发 Commit，等 CommitAck
    CommitSent,
    /// 成功切到 iroh
    Switched,
    /// 对方明确拒绝（版本不兼容等）
    Rejected(String),
    /// 超时或建连失败，已回退
    Failed(String),
}

impl State {
    /// 是否已到达终态。
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Switched | Self::Rejected(_) | Self::Failed(_))
    }

    /// 是否已经切到 iroh。
    pub fn is_switched(&self) -> bool {
        matches!(self, Self::Switched)
    }
}

/// 状态机要求调用方执行的动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// 在老通道上发送 `IrohUpgradeOffer`
    SendOffer,
    /// 在老通道上发送 `IrohUpgradeAnswer`
    SendAnswer { accepted: bool, reason: String },
    /// 主动拨号建立 iroh 连接（只由发起方执行）
    DialIroh,
    /// 在老通道上发送 `IrohUpgradeCommit`
    SendCommit,
    /// 在老通道上发送 `IrohUpgradeCommitAck`
    SendCommitAck,
    /// 执行真正的传输切换（**必须同时迁移加密状态**，见模块文档）
    SwitchTransport,
    /// 放弃升级，继续用老通道
    Fallback,
}

/// 驱动状态机的事件。
#[derive(Debug, Clone)]
pub enum Event<'a> {
    /// 本端决定开始升级（仅发起方）
    Start,
    /// 收到对方的 Offer
    OfferReceived { endpoint_addr: &'a str, version: u32 },
    /// 收到对方的 Answer
    AnswerReceived {
        accepted: bool,
        endpoint_addr: &'a str,
        reason: &'a str,
    },
    /// iroh 连接已建立**并已校验对端身份**
    IrohConnected,
    /// iroh 建立失败
    IrohFailed(&'a str),
    /// 某个阶段超时
    Timeout,
    /// 收到 Commit（响应方）
    CommitReceived,
    /// 收到 CommitAck（发起方）
    CommitAckReceived,
}

/// 升级状态机。
#[derive(Debug, Clone)]
pub struct Upgrade {
    role: Role,
    state: State,
    /// 对端给出的 iroh 地址（响应方在收到 Offer 时得到，发起方在收到 Answer 时得到）
    peer_addr: Option<String>,
    /// 本端的 iroh 地址，用于回给对端
    local_addr: String,
}

impl Upgrade {
    pub fn new(role: Role, local_addr: String) -> Self {
        Self {
            role,
            state: State::Idle,
            peer_addr: None,
            local_addr,
        }
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    pub fn role(&self) -> Role {
        self.role
    }

    pub fn peer_addr(&self) -> Option<&str> {
        self.peer_addr.as_deref()
    }

    /// 推进状态机，返回需要执行的动作。
    ///
    /// 终态下再投事件不会产生任何动作（幂等），避免重复切换。
    pub fn step(&mut self, event: Event<'_>) -> Vec<Action> {
        if self.state.is_terminal() {
            return Vec::new();
        }

        match (&self.state, event) {
            // ---------- 发起方开始 ----------
            (State::Idle, Event::Start) if self.role == Role::Initiator => {
                self.state = State::OfferSent;
                vec![Action::SendOffer]
            }

            // ---------- 响应方收到 Offer ----------
            (State::Idle, Event::OfferReceived { endpoint_addr, version })
                if self.role == Role::Responder =>
            {
                if version != UPGRADE_PROTOCOL_VERSION {
                    let reason = format!(
                        "升级协议版本不兼容：本端 {}，对端 {}",
                        UPGRADE_PROTOCOL_VERSION, version
                    );
                    self.state = State::Rejected(reason.clone());
                    return vec![Action::SendAnswer { accepted: false, reason }];
                }
                self.peer_addr = Some(endpoint_addr.to_owned());
                self.state = State::Connecting;
                // 先回 Answer（让发起方开始拨号），然后本端等对方连接进来
                vec![Action::SendAnswer {
                    accepted: true,
                    reason: String::new(),
                }]
            }

            // ---------- 发起方收到 Answer ----------
            (State::OfferSent, Event::AnswerReceived { accepted, endpoint_addr, reason }) => {
                if !accepted {
                    self.state = State::Rejected(reason.to_owned());
                    return vec![Action::Fallback];
                }
                self.peer_addr = Some(endpoint_addr.to_owned());
                self.state = State::Connecting;
                vec![Action::DialIroh]
            }

            // ---------- iroh 就绪 ----------
            (State::Connecting, Event::IrohConnected) => match self.role {
                // 发起方：连接一好就发 Commit
                Role::Initiator => {
                    self.state = State::CommitSent;
                    vec![Action::SendCommit]
                }
                // 响应方：等对方发 Commit，不主动切
                Role::Responder => {
                    self.state = State::AwaitingCommit;
                    Vec::new()
                }
            },

            // ---------- Commit / CommitAck ----------
            (State::AwaitingCommit, Event::CommitReceived) => {
                self.state = State::Switched;
                vec![Action::SendCommitAck, Action::SwitchTransport]
            }
            (State::CommitSent, Event::CommitAckReceived) => {
                self.state = State::Switched;
                vec![Action::SwitchTransport]
            }

            // ---------- 失败路径 ----------
            (_, Event::IrohFailed(e)) => {
                self.state = State::Failed(format!("iroh 建连失败：{e}"));
                vec![Action::Fallback]
            }
            (State::OfferSent, Event::Timeout) => {
                // 最典型的场景：对端是标准 RustDesk 客户端，不会回 Answer
                self.state = State::Failed("等待 Answer 超时（对端可能不支持升级）".to_owned());
                vec![Action::Fallback]
            }
            (State::Connecting, Event::Timeout) => {
                self.state = State::Failed("iroh 建连超时".to_owned());
                vec![Action::Fallback]
            }
            (State::CommitSent, Event::Timeout) => {
                self.state = State::Failed("等待 CommitAck 超时".to_owned());
                vec![Action::Fallback]
            }
            (State::AwaitingCommit, Event::Timeout) => {
                self.state = State::Failed("等待 Commit 超时".to_owned());
                vec![Action::Fallback]
            }

            // ---------- 其余组合都是协议违规，忽略但不改状态 ----------
            _ => Vec::new(),
        }
    }

    /// 把状态机当前应该发给对端的 Offer 载荷序列化出来。
    pub fn make_offer_payload(&self) -> ResultType<String> {
        if self.local_addr.is_empty() {
            return Err(anyhow!("本端 iroh 地址为空，无法发起升级"));
        }
        Ok(self.local_addr.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 完整握手：双方都必须切到 iroh，且各只切一次。
    #[test]
    fn happy_path_both_sides_switch() {
        let mut a = Upgrade::new(Role::Initiator, "addr-A".to_owned());
        let mut b = Upgrade::new(Role::Responder, "addr-B".to_owned());

        assert_eq!(a.step(Event::Start), vec![Action::SendOffer]);
        assert_eq!(a.state(), &State::OfferSent);

        assert_eq!(
            b.step(Event::OfferReceived { endpoint_addr: "addr-A", version: UPGRADE_PROTOCOL_VERSION }),
            vec![Action::SendAnswer { accepted: true, reason: String::new() }]
        );
        assert_eq!(b.state(), &State::Connecting);

        // 发起方拿到响应方地址 → 拨号
        assert_eq!(
            a.step(Event::AnswerReceived {
                accepted: true,
                endpoint_addr: "addr-B",
                reason: ""
            }),
            vec![Action::DialIroh]
        );
        assert_eq!(a.peer_addr(), Some("addr-B"));

        // 发起方 iroh 就绪 → 发 Commit
        assert_eq!(a.step(Event::IrohConnected), vec![Action::SendCommit]);
        assert_eq!(a.state(), &State::CommitSent);

        // 响应方 iroh 就绪 → 只等 Commit，**不得**自行切换
        assert_eq!(b.step(Event::IrohConnected), Vec::<Action>::new());
        assert_eq!(b.state(), &State::AwaitingCommit);

        // 响应方收到 Commit → 回 Ack 并切换
        assert_eq!(
            b.step(Event::CommitReceived),
            vec![Action::SendCommitAck, Action::SwitchTransport]
        );
        assert!(b.state().is_switched());

        // 发起方收到 Ack → 切换
        assert_eq!(
            a.step(Event::CommitAckReceived),
            vec![Action::SwitchTransport]
        );
        assert!(a.state().is_switched());

        // 终态幂等：再投事件不产生任何动作（防止重复切换）
        assert_eq!(a.step(Event::CommitAckReceived), Vec::<Action>::new());
        assert_eq!(b.step(Event::CommitReceived), Vec::<Action>::new());
    }

    /// 对端是标准 RustDesk 客户端：等不到 Answer，必须回退而不是卡死。
    #[test]
    fn unsupported_peer_times_out_and_falls_back() {
        let mut a = Upgrade::new(Role::Initiator, "addr-A".to_owned());
        a.step(Event::Start);
        let actions = a.step(Event::Timeout);
        assert_eq!(actions, vec![Action::Fallback]);
        assert!(matches!(a.state(), State::Failed(_)));
        assert!(!a.state().is_switched());
    }

    /// 版本不兼容必须干净拒绝，且**绝不能**切换。
    #[test]
    fn version_mismatch_is_rejected() {
        let mut b = Upgrade::new(Role::Responder, "addr-B".to_owned());
        let actions = b.step(Event::OfferReceived {
            endpoint_addr: "addr-A",
            version: UPGRADE_PROTOCOL_VERSION + 99,
        });
        match actions.as_slice() {
            [Action::SendAnswer { accepted: false, .. }] => {}
            other => panic!("期望拒绝应答，实际 {:?}", other),
        }
        assert!(matches!(b.state(), State::Rejected(_)));
    }

    /// 响应方在 iroh 就绪但**还没收到 Commit** 时绝不能切换 ——
    /// 否则两边切换时机不一致，老通道上的在途数据会丢。
    #[test]
    fn responder_never_switches_before_commit() {
        let mut b = Upgrade::new(Role::Responder, "addr-B".to_owned());
        b.step(Event::OfferReceived {
            endpoint_addr: "addr-A",
            version: UPGRADE_PROTOCOL_VERSION,
        });
        assert_eq!(b.step(Event::IrohConnected), Vec::<Action>::new());
        for action in b.step(Event::IrohConnected) {
            assert_ne!(action, Action::SwitchTransport);
        }
        assert!(!b.state().is_switched());
    }

    /// iroh 建连失败：双方都应回退，不能有人已经切过去。
    #[test]
    fn iroh_failure_falls_back_on_both_sides() {
        let mut a = Upgrade::new(Role::Initiator, "addr-A".to_owned());
        a.step(Event::Start);
        a.step(Event::AnswerReceived {
            accepted: true,
            endpoint_addr: "addr-B",
            reason: "",
        });
        assert_eq!(a.step(Event::IrohFailed("timeout")), vec![Action::Fallback]);
        assert!(!a.state().is_switched());

        let mut b = Upgrade::new(Role::Responder, "addr-B".to_owned());
        b.step(Event::OfferReceived {
            endpoint_addr: "addr-A",
            version: UPGRADE_PROTOCOL_VERSION,
        });
        assert_eq!(b.step(Event::IrohFailed("connection reset")), vec![Action::Fallback]);
        assert!(!b.state().is_switched());
    }

    /// 协议违规的事件组合不应改变状态。
    #[test]
    fn out_of_order_events_are_ignored() {
        let mut a = Upgrade::new(Role::Initiator, "addr-A".to_owned());
        // 还没 Start 就收到 Ack
        assert_eq!(a.step(Event::CommitAckReceived), Vec::<Action>::new());
        assert_eq!(a.state(), &State::Idle);
        // 发起方收到 Offer 也不该有反应
        assert_eq!(
            a.step(Event::OfferReceived { endpoint_addr: "x", version: 1 }),
            Vec::<Action>::new()
        );
        assert_eq!(a.state(), &State::Idle);
    }

    /// 空地址必须被拒绝，而不是发出一个对方无法连接的 Offer。
    #[test]
    fn empty_local_addr_is_rejected() {
        let u = Upgrade::new(Role::Initiator, String::new());
        assert!(u.make_offer_payload().is_err());
    }
}
