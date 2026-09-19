//! **端到端**验证：两个真实的 `UpgradeSession` 在**真实 iroh 连接**上
//! 走完整个升级握手，最后真的把数据从 QUIC 通道上送过去。
//!
//! 为什么需要这个测试
//! ------------------
//! 已有的测试各自只覆盖一段，中间那一大块是空的：
//!
//! | 已有测试 | 覆盖 | 缺口 |
//! |---|---|---|
//! | `iroh_upgrade.rs` | 纯状态机 | 没有真实 iroh |
//! | `iroh_upgrade_session.rs` | 会话驱动（假的 tokio duplex 流） | 没有真实 iroh |
//! | `iroh_transport.rs` | 真实 iroh 裸往返 | 没有升级握手 |
//! | `iroh_transition.rs` / `crypto_handoff.rs` | 过渡层与 nonce 迁移 | 没有真实 iroh |
//!
//! 于是"两边能不能真的握上手"这件事，从来没有被验证过 ——
//! 而它恰恰是最容易出问题的一环（地址交换、拨号方向、时序）。
//!
//! 这个测试用**纯公开 API**（连接层能看到的就是这些），
//! 把 `connection.rs` / `io_loop.rs` 里那段驱动的循环照搬过来，
//! 两个会话互相投递真实的 protobuf 字节。
//!
//! 它**不能**替代两台真机的实测（没有经过 hbbs 信令、没有真实网络路径），
//! 但它把"协议逻辑 + iroh 连接"这一段从"没验证过"变成"已验证"。

#![cfg(feature = "iroh-transport")]

use std::time::Duration;

use hbb_common::iroh_transition::TransitionStream;
use hbb_common::iroh_upgrade::{Role, State};
use hbb_common::iroh_upgrade_session::{IncomingUpgrade, Order, UpgradeSession};
use hbb_common::iroh_transport::IrohConfig;
use hbb_common::message_proto::Message;
use hbb_common::protobuf::Message as _;
use hbb_common::stream::Stream;
use hbb_common::tcp::FramedStream;

/// 本地直连配置：绑 loopback、**关掉 relay**（纯打洞/直连路径）。
fn local_cfg() -> IrohConfig {
    IrohConfig {
        bind_addr: Some("127.0.0.1:0".to_owned()),
        relay_urls: Vec::new(),
        secret_key: None, // iroh 随机生成 -> 两端身份不同
        ..Default::default() // relay_auth_token 等新增字段取默认（测试不需要令牌）
    }
}

/// 一端：会话 + 收集到的指令。
struct Side {
    name: &'static str,
    session: UpgradeSession,
    /// 装上的过度层里的新流（就是那条 iroh QUIC 连接）
    installed: Option<FramedStream>,
    outbox: Vec<Message>,
    /// 指令序列，用来断言顺序
    trace: Vec<&'static str>,
}

impl Side {
    fn new(name: &'static str, role: Role) -> Self {
        Self {
            name,
            session: UpgradeSession::new(role, local_cfg()),
            installed: None,
            outbox: Vec::new(),
            trace: Vec::new(),
        }
    }

    /// 照 `connection.rs` / `io_loop.rs` 的做法执行会话给出的指令。
    async fn run_orders(&mut self, orders: Vec<Order>) {
        for order in orders {
            match order {
                Order::Send(m) => self.outbox.push(m),
                Order::Install(stream) => {
                    self.trace.push("Install");
                    assert!(
                        self.installed.is_none(),
                        "{} 装了两次过渡层，说明指令重复了",
                        self.name
                    );
                    self.installed = Some(stream);
                }
                Order::CommitSent => self.trace.push("CommitSent"),
                Order::CommitAckSent => self.trace.push("CommitAckSent"),
                Order::CommitAckReceived => self.trace.push("CommitAckReceived"),
                Order::Abort => self.trace.push("Abort"),
            }
        }
    }

    fn take_outbox(&mut self) -> Vec<Message> {
        std::mem::take(&mut self.outbox)
    }

    /// 把一端的待发消息**真的序列化成 protobuf 字节再解析回来**，
    /// 投给另一端。这样连"消息能不能过线"也一起验证了。
    async fn deliver_to(&mut self, msgs: &[Message]) {
        for m in msgs {
            let bytes = m.write_to_bytes().expect("序列化升级消息失败");
            let parsed = Message::parse_from_bytes(&bytes).expect("反序列化升级消息失败");
            let union = parsed.union.as_ref().expect("消息没有 union");
            let incoming = IncomingUpgrade::from_union(union)
                .unwrap_or_else(|| panic!("{} 收到的不是升级消息", self.name));
            let orders = self
                .session
                .handle(&incoming)
                .await
                .unwrap_or_else(|e| panic!("{} 处理升级消息失败: {e}", self.name));
            self.run_orders(orders).await;
        }
    }
}

/// 走完整个握手，返回两端（都应当已切到 iroh）。
async fn handshake(timeout: Duration) -> (Side, Side) {
    let mut init = Side::new("发起方", Role::Initiator);
    let mut resp = Side::new("响应方", Role::Responder);

    // ---- 1) 发起方 start -> Offer ----
    eprintln!("[e2e] 1) 发起方 start（建 endpoint）...");
    let orders = init.session.start().await.expect("start 失败");
    eprintln!("[e2e]    完成，指令数={}", orders.len());
    init.run_orders(orders).await;
    let offer = init.take_outbox();
    assert_eq!(offer.len(), 1, "start 应当只产出一条 Offer");

    // ---- 2) 响应方收到 Offer -> Answer ----
    eprintln!("[e2e] 2) 响应方处理 Offer（建 endpoint）...");
    resp.deliver_to(&offer).await;
    eprintln!("[e2e]    完成");
    let answer = resp.take_outbox();
    assert_eq!(answer.len(), 1, "收到 Offer 应当只回一条 Answer");

    // ---- 3) 发起方收到 Answer -> 内部开始拨号 ----
    eprintln!("[e2e] 3) 发起方处理 Answer（触发拨号）...");
    init.deliver_to(&answer).await;
    eprintln!("[e2e]    完成");

    // ---- 4) 驱动到双方都 Switched ----
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if init.session.state().is_switched() && resp.session.state().is_switched() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "升级超时：发起方={:?} 响应方={:?}\n\
             发起方指令={:?}\n响应方指令={:?}",
            init.session.state(),
            resp.session.state(),
            init.trace,
            resp.trace
        );

        // 两端各驱动一次（对应两个 select! 循环里的定时分支）。
        // poll_* 只返回"这次有没有拿到连接"，指令要另外 drain —— 与
        // connection.rs 的 drive_iroh_upgrade() 写法完全一致。
        init.session.poll_dial().await.expect("poll_dial 失败");
        let o = init.session.drain_orders();
        init.run_orders(o).await;
        resp.session.poll_accept().await.expect("poll_accept 失败");
        let o = resp.session.drain_orders();
        resp.run_orders(o).await;

        // 互相投递这一轮产生的消息（Commit / CommitAck）
        let to_resp = init.take_outbox();
        let to_init = resp.take_outbox();
        resp.deliver_to(&to_resp).await;
        init.deliver_to(&to_init).await;

        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    (init, resp)
}

// ===========================================================================
// 测试
// ===========================================================================

/// 主验收点：两个真实 endpoint 之间完成升级，双方都切到 iroh。
#[tokio::test]
async fn two_sides_complete_upgrade_over_real_iroh() {
    let _ = env_logger::builder().is_test(true).try_init();
    let (init, resp) = handshake(Duration::from_secs(30)).await;

    assert_eq!(init.session.state(), &State::Switched, "发起方没有切到 iroh");
    assert_eq!(resp.session.state(), &State::Switched, "响应方没有切到 iroh");

    // 双方都必须装过过渡层，且只装一次
    assert!(init.installed.is_some(), "发起方没拿到 iroh 流");
    assert!(resp.installed.is_some(), "响应方没拿到 iroh 流");

    // 身份绑定：iroh 身份就是 RustDesk 设备身份
    let ip = init.session.peer_id().expect("发起方不知道对端 iroh 身份");
    let rp = resp.session.peer_id().expect("响应方不知道对端 iroh 身份");
    assert_ne!(ip, rp, "两端身份不该相同（否则是连到了自己）");
}

/// 顺序验收点（这是 run #17 之前修掉的那个静默丢数据 bug 的守卫）：
/// 发起方在连接就绪后必须是 **先 Install、再 Send(Commit)、最后 CommitSent**。
#[tokio::test]
async fn initiator_install_precedes_commit() {
    let (init, _resp) = handshake(Duration::from_secs(30)).await;
    assert_eq!(
        init.trace,
        vec!["Install", "CommitSent", "CommitAckReceived"],
        "发起方指令顺序不对（Send(Commit) 不计入 trace）：{:?}",
        init.trace
    );
}

/// 响应方不得在收到 Commit 之前切换：它的 trace 应当是
/// `Install -> CommitAckSent`，而不是先切换。
#[tokio::test]
async fn responder_acks_before_it_is_switched() {
    let (_init, resp) = handshake(Duration::from_secs(30)).await;
    assert_eq!(
        resp.trace,
        vec!["Install", "CommitAckSent"],
        "响应方指令顺序不对：{:?}",
        resp.trace
    );
}

/// **最终验收点**：切换之后，数据真的能从一条 iroh 流走到另一条。
///
/// 这一步之前没有任何测试覆盖 —— "状态机说切换了"和
/// "数据真的过去了"是两件事。
#[tokio::test]
async fn data_flows_over_the_switched_channel() {
    let _ = env_logger::builder().is_test(true).try_init();
    let (mut init, mut resp) = handshake(Duration::from_secs(30)).await;

    let mut a = init.installed.take().expect("发起方没有 iroh 流");
    let mut b = resp.installed.take().expect("响应方没有 iroh 流");

    // 方向 1：发起方 -> 响应方
    a.send_bytes(bytes::Bytes::from_static(b"hello-over-quic")).await
        .expect("往 iroh 流写失败");
    let got = tokio::time::timeout(Duration::from_secs(10), b.next())
        .await
        .expect("读 iroh 流超时")
        .expect("iroh 流已关闭")
        .expect("读 iroh 流出错");
    assert_eq!(&got[..], b"hello-over-quic", "发起方->响应方的数据不对");

    // 方向 2：响应方 -> 发起方（证明是同一条双向连接）
    b.send_bytes(bytes::Bytes::from_static(b"and-back-again")).await
        .expect("往 iroh 流写失败");
    let got = tokio::time::timeout(Duration::from_secs(10), a.next())
        .await
        .expect("读 iroh 流超时")
        .expect("iroh 流已关闭")
        .expect("读 iroh 流出错");
    assert_eq!(&got[..], b"and-back-again", "响应方->发起方的数据不对");
}

/// 过渡层装好之后，**应用数据必须真的从 iroh 走**，而不是还留在老通道上。
///
/// 这个测试把连接层的做法完整搬过来一次：用一条假的"老流"造出
/// `TransitionStream`，模仿 `Order::CommitSent` / `CommitAckReceived` 的时序，
/// 最后确认数据出现在对端的 iroh 流上。
#[tokio::test]
async fn transition_layer_actually_carries_data_over_iroh() {
    let _ = env_logger::builder().is_test(true).try_init();
    let (mut init, mut resp) = handshake(Duration::from_secs(30)).await;

    let new_a = init.installed.take().expect("发起方没有 iroh 流");
    let mut b = resp.installed.take().expect("响应方没有 iroh 流");

    // 造一条"老通道"（就是本机的一条 TCP 连接，够用了）
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (old_client, old_server) = tokio::join!(
        async {
            let s = tokio::net::TcpStream::connect(addr).await.unwrap();
            Frame::wrap(s)
        },
        async {
            let (s, _) = listener.accept().await.unwrap();
            Frame::wrap(s)
        }
    );
    let mut old_peer = old_server;

    // 按连接层的做法装过渡层。
    // 注意 `TransitionStream::new` 的第一参数是 `Stream` 而不是 `FramedStream`。
    let mut trans = TransitionStream::new(Stream::Tcp(old_client), new_a);

    // ---- 提交窗口：先发 Commit（进窗口），窗口内的写入必须被暂存 ----
    trans.on_commit_sent();
    trans
        .send_bytes(bytes::Bytes::from_static(b"buffered-in-commit-window"))
        .await
        .expect("过渡层写入失败");
    assert_eq!(trans.buffered_len(), 1, "提交窗口内的写入应当被暂存");
    let stray = tokio::time::timeout(Duration::from_millis(300), old_peer.next()).await;
    assert!(
        stray.is_err(),
        "提交窗口内的数据不该走老通道，却收到了 {:?}",
        stray.ok().flatten()
    );

    // ---- 收到 CommitAck：换到新通道并冲刷暂存数据 ----
    trans.on_commit_ack_received();
    trans.flush_to_new().await.expect("冲刷到新通道失败");

    let got = tokio::time::timeout(Duration::from_secs(10), b.next())
        .await
        .expect("读 iroh 流超时")
        .expect("iroh 流已关闭")
        .expect("读 iroh 流出错");
    assert_eq!(
        &got[..],
        b"buffered-in-commit-window",
        "提交窗口内暂存的数据没有走到 iroh 通道上"
    );

    // ---- 切换之后写的数据必须走新通道，老通道上不该再有数据 ----
    trans
        .send_bytes(bytes::Bytes::from_static(b"after-switch"))
        .await
        .expect("过渡层写入失败");
    let got = tokio::time::timeout(Duration::from_secs(10), b.next())
        .await
        .expect("读 iroh 流超时")
        .expect("iroh 流已关闭")
        .expect("读 iroh 流出错");
    assert_eq!(&got[..], b"after-switch");

    let stale = tokio::time::timeout(Duration::from_millis(300), old_peer.next()).await;
    assert!(
        stale.is_err(),
        "切换之后老通道上不该再有数据，却收到了 {:?}",
        stale.ok().flatten()
    );
}


/// 小工具：把一条 TCP 流包成 `FramedStream`。
struct Frame;

impl Frame {
    fn wrap(s: tokio::net::TcpStream) -> FramedStream {
        use hbb_common::bytes_codec::BytesCodec;
        use hbb_common::tcp::DynTcpStream;
        use tokio_util::codec::Framed;
        FramedStream(
            Framed::new(DynTcpStream(Box::new(s)), BytesCodec::new()),
            "127.0.0.1:0".parse().unwrap(),
            None,
            0,
        )
    }
}


/// 隔离微测试：`try_accept` 自称"非阻塞，ms 毫秒内没有连接就返回 None"。
/// 空 endpoint 上必须立刻返回 —— 它此前从未被任何测试覆盖过。
#[tokio::test]
async fn try_accept_is_actually_nonblocking() {
    let ep = hbb_common::iroh_transport::new_endpoint_from_config(local_cfg())
        .await
        .expect("建 endpoint 失败");
    let t0 = std::time::Instant::now();
    let got = hbb_common::iroh_transport::try_accept(&ep, 50)
        .await
        .expect("try_accept 出错");
    let dt = t0.elapsed();
    eprintln!("[micro] try_accept 用时 {:?}, 结果 is_none={}", dt, got.is_none());
    assert!(got.is_none(), "空 endpoint 不该收到连接");
    assert!(
        dt < std::time::Duration::from_secs(3),
        "try_accept 没有在 50ms 后返回，实际用了 {dt:?}"
    );
}
