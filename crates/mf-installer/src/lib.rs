//! mf-installer:CLI 发现/adoption 与 Provider 模型探测(T4b,Issue #40;
//! canonical spec §9.4/§9.8)。
//!
//! 本 crate 交付 discovery(宿主 PATH + 受管根、canonical 去重、
//! shim/symlink target 识别)、可信 artifact adoption、Provider remote
//! catalog probe(超时/重试/缓存/回退/手填校验)与附录 A5 limits。
//! 安装计划/执行/receipt 属 #43(T4c);真实 HTTP transport 随 WebGateway。
//!
//! 安全不变式:浏览器输入不能变成搜索路径(candidate 只接受命令名);
//! Provider Secret 只在 Core 内解析(probe 结果只含模型元数据与缓存
//! 状态,绝不回传凭据)。

pub mod adoption;
pub mod discovery;
pub mod executor;
pub mod limits;
pub mod plan;
pub mod provider_probe;
pub mod receipt;
