[中文](README.zh-CN.md) | **English**

# onvif-rs

[![CI](https://github.com/mickeyzzc/onvif-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/mickeyzzc/onvif-rs/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![Language: Rust](https://img.shields.io/badge/language-Rust-dea584.svg)
![Tests](https://img.shields.io/badge/tests-188%20passing-brightgreen.svg)

**ONVIF 设备端（服务端）Rust 库** —— 通过 SOAP + WS-Discovery 把摄像头或媒体源暴露给 ONVIF 消费方（NVR、视频管理平台）。

> **命名说明**：本项目与 [lumeohq/onvif-rs](https://github.com/lumeohq/onvif-rs)（WSDL 生成的 ONVIF *客户端*）无关。crates.io 的 `onvif-rs` 名字被一个 2018 年的废弃占位仓持有，因此 crate 以 **`onvif-device-rs`** 名义发布；仓库保留原名。

## 功能

- **SOAP HTTP 服务端** —— 按动作注册处理器，覆盖 Device、Media、Imaging 与（虚拟）PTZ 服务；Device 服务补齐 devicemgmt 读写全表面：**SetSystemDateAndTime**、可变 **scopes**（Add/Remove/Set；GetScopes 应答 WSDL `tds:Scopes`/`tt:ScopeDef`/`tt:ScopeItem` 形态——内建为 `Fixed`、客户端添加为 `Configurable`）、**Get/SetHostname**、**Get/SetDiscoveryMode**、**用户目录**（Get/Create/Delete/SetUser —— 仅目录，认证真源仍是 WS-Security 层；经 `with_users` 播种）、诚实的网络静态应答（GetDNS/GetNTP/GetNetworkInterfaces/GetNetworkDefaultGateway/GetNetworkProtocols）、GetServiceCapabilities/GetWsdlUrl/GetEndpointReference、**GetSystemLog/GetSystemSupportInformation**，以及"仅协议答复"家族（**SystemReboot**、SetSystemFactoryDefault、UpgradeSystemFirmware、StartSystemRestore）——真实副作用归宿主，经 `DeviceHooks` 接缝（`with_hooks`）注入；Set* 网络族、证书与 802.1X 刻意未实现；GetServices/GetCapabilities 只枚举宿主真正提供的服务 —— Media/PTZ/Imaging 由 `with_media_support`/`with_ptz_support`/`with_imaging_support` 开关控制（默认开），Events 由 `with_events_support` 控制
- **WS-Discovery 应答器** —— UDP 组播 239.255.255.250:3702 Probe/ProbeMatches，按请求回显 XAddr；scopes 与 EndpointReference UUID 均可由宿主配置；**启动即发 Hello、停机即发 Bye** 主动通告（与 ProbeMatches 同族信封）
- **WS-Security** —— UsernameToken 校验，PasswordText 与 PasswordDigest（SHA-1），常数时间比较，空密码 **fail-closed** 处理
- **安全补全**（issue #54）—— 在 WS-Security 之上补三项：**HTTP Digest 传输认证**（RFC 7616 子集：MD5、`qop="auth"`；`OnvifConfig::http_digest` 选择启用 —— 无令牌请求收到 401 `WWW-Authenticate: Digest` 挑战，合法 Digest 头无需 UsernameToken 即可认证，两种机制可并存且存在 UsernameToken 时以它为准；服务端 nonce 300 秒过期并按 nonce 强制 nc 递增，截获的 `Authorization` 头不可重放）；**IP 地址过滤** —— `DeviceServiceHandlers::with_ip_filter` 与 `OnvifServer::with_ip_filter` 接同一个共享存储：Get/Set/Add/RemoveIPAddressFilter 经 SOAP 改动它，服务器对被拒来源在任何 HTTP/SOAP 处理之前回 403（IPv4 前缀、Allow/Deny 模式；无法解析/IPv6 的对端地址失败开放并记日志）；**Get/SetAccessPolicy** —— 透明 Base64 `PolicyFile` 块原样存取（`with_access_policy`；策略的解释执行属宿主侧）。802.1X 刻意不做（EAP supplicant 属宿主基础设施而非 SOAP 协议 —— 相关动作按通用不支持动作应答）
- **TLS 监听**（可选 `tls` feature，默认关闭）—— 配置证书/私钥 PEM 路径后以 HTTPS 提供 ONVIF 服务（Profile T 传输基线）；两者必须同时设置（both-or-neither）
- **事件 pull-point 服务**（对齐 onvif-go 的 `SupportEvents`）—— `server.enable_events()` 后，服务器在自有监听器上路由 `/onvif/events_service`（GetServiceCapabilities / GetEventProperties / CreatePullPointSubscription）与 `/onvif/events_service/sub/<id>`（PullMessages / Renew / Unsubscribe）；宿主通过返回的 `EventsService::publish_event` 接缝注入事件（扇出到全部在订订阅；Concrete/ConcreteSet 主题过滤、ISO8601 终止时间钳制、长轮询 PT0S 合法）；GetCapabilities/GetServices 仅在启用时通告该服务，Create* 动作走 WS-Security 而读操作开放
- **事件基础通知接口**（`wsnt:Subscribe`）—— 与 pull-point 并列的推送订阅：`Subscribe` 登记消费者（ConsumerReference 仅支持 `http://` URL——https 消费者直接拒绝，库不携带 TLS 客户端），每个订阅一个发送任务，把每条匹配的已发布事件以 `wsnt:Notify` SOAP 文档 POST 给消费者（Topic 与内层报文写入器与 PullMessages 应答同源、携带 SubscriptionReference、省略 ProducerReference）；投递 fire-and-forget（连接/读写各 5 秒超时），连续 3 次失败自动退订该订阅（规范允许的清理动作）；Renew/Unsubscribe 复用 pull-point 生命周期与 10 个订阅的共享上限；`SetSynchronizationPoint` 在事件端点空应答（路由式分发，与 media 同名动作不冲突）；GetEventInstances 刻意不实现（17.06+ 特性，无需求）
- **成像对焦控制** —— Move（经 `ImagingParams::focus_move` 接缝的绝对/相对/连续对焦）、Stop、GetMoveOptions、GetStatus、GetServiceCapabilities；共享动作名（`GetStatus`/`Stop`/`GetServiceCapabilities`）按请求形状路由，未命中回落先前注册的 handler（PTZ）
- **命名空间无关的请求解析**（客户端可用任意 XML 前缀）与显式前缀序列化（`tds:`/`trt:`/`timg:`/`tt:`），所有插值均做 XML 转义
- **多媒体 Profile**（主码流 + 子码流）—— `OnvifMediaConfig::extra_profiles` 在 GetProfiles 中追加 `MediaProfileConfig` 条目（如 640×360 省流子码流），恒排在主 Profile 之后；GetStreamUri 按请求的 `ProfileToken` 匹配并返回对应 RTSP 路径，未知/缺失 token 一律回落主流——单 Profile 客户端行为不变
- **Media 服务补全** —— `register_media_actions(server, Arc<RwLock<OnvifMediaConfig>>, keyframe_hook)` 一次注册整个服务：视频编码器配置族（GetVideoEncoderConfigurations / GetVideoEncoderConfiguration / GetVideoEncoderConfigurationOptions / **SetVideoEncoderConfiguration**——写入落入共享存储，GetProfiles 等全部读取方立即可见）、GetGuaranteedNumberOfVideoEncoderInstances、**SetSynchronizationPoint**（触发宿主的关键帧/IDR 钩子）、Media GetServiceCapabilities（SnapshotUri 跟随 `snapshot_port`）、以及音频/OSD 空集应答（无音频硬件、无 OSD 引擎——诚实的空答复而非Fault）；组播流媒体刻意不实现并在能力中如实关闭（`RTPMulticast="false"`）
- **Media2 服务（ver20/media，`tr2`）** —— Profile T 入口路径（对齐 onvif-go 的 `SupportMedia2`）：`server.enable_media2(store, keyframe_hook)` 在自有监听器上以**独立分发**路由 `/onvif/media2_service`（与 Media1 同名动作在共享动作表里冲突，由 URL 决定哪个面应答）；GetProfiles（可选 Token 过滤，`tr2:Profiles`/`tr2:Configurations` 外层 + `tt:` 配置体）、GetStreamUri/GetSnapshotUri（纯 `tr2:Uri` 形态）、视频编码器配置族与选项（tr2 元素形态、子元素式分辨率表）、GetVideoEncoderInstances、**SetSynchronizationPoint**（同一关键帧钩子）、Media2 GetServiceCapabilities（RTSPStreaming=true）；读写同一个 `SharedMediaConfig` 存储——经任一面 SetVideoEncoderConfiguration 两侧立即可见；Set* 动作走 WS-Security、读操作开放；GetServices 通告由 `with_media2_support` 控制（默认关，旧字节不变；GetCapabilities 传统枚举无 Media2 槽位，不涉及）
- **虚拟 PTZ** —— 纯状态机（`ptz_state`）支撑无云台设备的 PTZ 服务：转动、预置位、**Home 位**（Set/Goto）、配置存储（**SetConfiguration** 由 GetConfigurations 反射）、**GetConfigurationOptions**（坐标空间 + 超时）、**SendAuxiliaryCommand**（应答 + 回显）、**GetPTZServiceCapabilities**
- **优雅停机** —— SOAP 服务端与 discovery 应答器均支持

代码抽取自 [mibee-eye-raspi-rs](https://github.com/Mi-Bee-Studio) 的生产实现，其响应 XML 对 **MiBee NVR 逐字节稳定**（NVR 按本地名做原始 SOAP 匹配）。元素名遵循官方 WSDL（`GetStreamUriResponse → MediaUri → Uri`）。

## 使用

```toml
[dependencies]
onvif-device-rs = "0.3.0"
# git 替代方式: onvif-device-rs = { git = "https://github.com/mickeyzzc/onvif-rs.git", tag = "v0.3.0" }
```

```rust,no_run
use std::sync::Arc;
use onvif_device_rs::config::DeviceConfig;
use onvif_device_rs::device::{DeviceHandler, DeviceServiceHandlers};
use onvif_device_rs::discovery::DiscoveryServer;
use onvif_device_rs::imaging::{register_imaging_actions, ImagingParamError, ImagingParams};
use onvif_device_rs::media::{
    GetProfilesHandler, GetSnapshotUriHandler, GetStreamUriHandler, OnvifMediaConfig,
};
use onvif_device_rs::server::{OnvifConfig, OnvifServer};

// 在你的摄像头参数管理器上实现 Imaging 接缝。
struct MyParams;
impl ImagingParams for MyParams {
    fn get_param(&self, name: &str) -> Result<f64, ImagingParamError> { todo!() }
    fn set_param(&self, name: &str, value: f64) -> Result<(), ImagingParamError> { todo!() }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let device_ip = "192.0.2.10".to_string();
    let port = 8080u16;

    // 鉴权 fail-closed：空密码且未显式 allow_no_auth = true 时，
    // start() 直接报错，而不是静默把服务端变成全开放。
    let config = OnvifConfig {
        port,
        username: "admin".to_string(),
        password: "set-a-real-password".to_string(),
        ..Default::default()
    };
    // TLS（可选 `tls` feature）：再设 tls_cert_file + tls_key_file 两个
    // PEM 路径，start() 即以 https 提供服务（两者必须同时设置）。
    let mut server = OnvifServer::new(&config);

    // Device 服务：身份信息来自 DeviceConfig（宿主提供）。
    // 中性 "unknown" 占位会被校验拒绝——必须显式配置真实身份（issue #20）。
    let device = Arc::new(DeviceServiceHandlers::new(
        DeviceConfig {
            name: "My Camera".into(),
            manufacturer: "My Company".into(),
            model: "Cam-X".into(),
            firmware: "1.0.0".into(),
            hardware_id: "cam-x".into(),
            serial_number: "SN-001".into(),
        },
        port,
        device_ip.clone(),
    )
    .expect("explicit device identity"));
    for action in ["GetSystemDateAndTime", "GetDeviceInformation",
                   "GetCapabilities", "GetServices", "GetScopes"] {
        // 按 ONVIF 规范该动作免鉴权：
        if action == "GetSystemDateAndTime" {
            server.register_anonymous_action(action);
        }
        server.register_handler(action, Box::new(DeviceHandler(Arc::clone(&device))));
    }

    // Media 服务：token、编码（H264/H265）、流与快照 URI 全部是
    // OnvifMediaConfig 的字段。
    let media = Arc::new(OnvifMediaConfig::new(1920, 1080, 25, 2_000_000, 8554, device_ip.clone()));
    server.register_handler("GetProfiles", Box::new(GetProfilesHandler::new(Arc::clone(&media))));
    server.register_handler("GetStreamUri", Box::new(GetStreamUriHandler::new(Arc::clone(&media))));
    server.register_handler("GetSnapshotUri", Box::new(GetSnapshotUriHandler::new(media)));

    // Imaging 服务用注册辅助函数；PTZ 同理（见 examples）。
    register_imaging_actions(&mut server, Arc::new(MyParams));

    // 启动两个服务；返回的句柄支持优雅停机。
    let mut soap = server.start().await?;               // 0.0.0.0:port
    let mut discovery = DiscoveryServer::with_identity(&device_ip, port, "My Camera", "Model X")
        .start()
        .await?;                                        // udp/3702 组播

    // ... 运行你的应用 ...
    discovery.shutdown().await?;
    soap.shutdown().await?;
    Ok(())
}
```

`DiscoveryServer::with_uuid` 可固定 EndpointReference UUID，避免按它识别设备的 NVR 把每次重启当成新设备。本 crate 通过 [`log`](https://crates.io/crates/log) facade 输出日志 —— 宿主初始化 logger 后才能看到输出。

完整接线见 [`examples/device_demo.rs`](examples/device_demo.rs)（它同时是 README 式代码的编译验证参照），生产接线见 `mibee-eye-raspi-rs` 的 `main.rs`。

## 文档

专题手册已迁移至 MiBee 文档中心——库手册唯一真源（双语）：

> **https://www.mlsbs.top/docs/mibeelibs**

手册修改向文档中心提 PR（流程见该仓 GOVERNANCE）。本仓 [`docs/README.md`](docs/README.md) 保留引导页。
## 库卫生（v0.3.0 加固）

v0.3.0 把本 crate 打磨为可放心嵌入的中性基础库。[`tests/library_hygiene.rs`](tests/library_hygiene.rs) 与 [`tests/server_lifecycle.rs`](tests/server_lifecycle.rs) 中的回归测试逐条锁定以下保证：

- **鉴权 fail-closed** —— 空密码不再静默关闭鉴权；除非显式设置 `allow_no_auth = true`，`start()` 直接报错。
- **中性的 discovery 身份** —— `DiscoveryServer::new` 只宣告规范 profile scope（移除了原硬件的 `PiCameraV1`/`OV5647` scope）；用 `with_identity`/`with_scopes` 自定义。scope 值做百分号编码（空格安全）。
- **UUID 可持久化** —— `with_uuid` 让 discovery 身份跨重启稳定。
- **XML 转义** —— 客户端可控值（SOAP 动作名、预置位名、Probe MessageID）与宿主配置字符串在响应中转义。
- **Media 面可配置** —— profile/源/编码器 token、编码（H.264 **或 H.265**）、GetVideoSources 名称均来自 `OnvifMediaConfig`。
- **Imaging 模式可配置** —— `ImagingParams::exposure_mode` / `white_balance_mode`（带默认实现，向后兼容）。
- **HTTP 加固** —— 头部多段读取（16 KiB 上限）、默认 1 MiB 请求体上限（超限 413 并先排空再关闭）、每连接读超时（默认 30 s）。
- **优雅停机 + 监听器注入** —— `OnvifServerHandle`/`DiscoveryHandle` 提供 `shutdown()`；`OnvifServer::start_on(listener)` 接受预先绑定的监听器。
- **`log` facade** —— 库代码零 `println!`/`eprintln!`；锁中毒做恢复而非级联 panic。

### 0.2.x → 0.3.0 破坏性变更

- `OnvifConfig` 新增 `allow_no_auth`、`max_body_bytes`、`read_timeout`（字面量请用 `..Default::default()`）。
- `OnvifServer::start` 返回 `OnvifServerHandle`（原来是 `()`）；`DiscoveryServer::start` 返回 `DiscoveryHandle`。
- `DiscoveryServer::new` 接受 `&str`，且默认不再宣告 name/hardware scope。
- `GetVideoSources` 名称默认为 `Video Source`（原来是 `Pi Camera`）。
- 库输出从 stdout/stderr 迁移到 `log` facade。

## 示例

[`examples/`](examples/) 提供可运行的自检演示：

```sh
cargo run --example device_demo [-- --port 8080] [-- --serve]
```

它启动真实 SOAP 服务端 + WS-Discovery 应答器，再像外部 ONVIF 客户端一样驱动它们：匿名 `GetSystemDateAndTime`、无凭据时 `GetDeviceInformation` 被 401 拒绝、带 WS-Security UsernameToken 摘要（演示内独立计算 SHA-1）时通过、`GetCapabilities`、`GetProfiles`/`GetStreamUri`/`GetSnapshotUri`、以及 UDP 上的 WS-Discovery Probe。全部通过后以 0 退出 —— 无需硬件的整机冒烟测试。`--serve` 保持服务运行以便手动验证（curl / ONVIF Device Manager / NVR）。
另外两个聚焦服务的演示遵循同样模式（真实服务端 + 外部客户端检查 + 成功退出 0）：

```sh
cargo run --example ptz_demo      # move/status/preset 动词 + 模拟运动
cargo run --example imaging_demo  # imaging 参数读写 + 越界错误
```

## 字节稳定性保证

MiBee NVR 这类消费方按本地元素名在原始字节流上匹配 SOAP 响应。本 crate 的序列化是承重结构：**改动响应元素名、命名空间前缀或属性顺序前必须重跑消费方互操作测试**。crate 的测试中包含锁定该性质的 golden 响应串。

## 开发

本项目严格执行 **TDD**，见 [CONTRIBUTING.md](CONTRIBUTING.md)。CI 强制 `rustfmt`、`clippy -D warnings` 与全量测试（157 个，含 golden 响应串）；`main` 分支受保护（仅 PR 合入，CI 必过）。

## 状态

v0.3.0 —— 接缝（`ImagingParams`、`DeviceConfig`、media 注册）趋于稳定但尚未冻结。在 [Mi-Bee Studio](https://github.com/Mi-Bee-Studio) 每日对 MiBee NVR 生产验证。

## 许可

MIT —— 见 [LICENSE](LICENSE)。代码抽取自 Mi-Bee Studio 摄像头项目。
