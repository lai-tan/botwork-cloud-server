// Copyright (c) 2026 Jehu Shaw. All rights reserved.

//! §4.3 凭据 SSH stdin 握手通道（cloud-server v0.2.0 侧消费实现）。
//!
//! 与 bot-work-windows 侧 `credential_channel.rs` 协议对称（同一 magic 前缀 /
//! 同一 64KB 上限 / 同一 JSON 载荷 / 同一凭据键集合）：
//! - one-shot（claude 网关模式 / openclaw 包装模式）：stdin 首行
//!   `BOTWORK_CREDENTIALS:{json}`（bot-work-windows 发送，本侧消费后驻留内存，
//!   经子进程 env 注入，cloud-server 自身 environ 不含 key）
//! - MCP 长连接（claude 模式）：initialize 请求行 params.credentials 扩展
//!   字段，消费后剥离转发（claude CLI 收到的 initialize 不含 credentials）
//! - codex 模式：进程内 app-server，经 vendored `InitializeParams` 扩展字段
//!   直达（app-server-protocol/protocol/v1.rs，§4.3-5e）
//!
//! 驻留约束（design §4.3）：凭据仅存 Rust 内存，cloud-server 自身进程禁止
//! `std::env::set_var`、禁止写文件、禁止日志打印；凭据仅允许注入子进程 env
//! （延迟 spawn 时点，仅存在于子进程启动时刻）。

use std::collections::HashMap;

/// 凭据键集合（与 bot-work-windows credential_channel::CREDENTIAL_KEYS 对称）
pub const CREDENTIAL_KEYS: [&str; 3] = ["OPENAI_API_KEY", "AGNES_API_KEY", "BOTWORK_PROVIDERS"];

/// one-shot stdin 首行 magic 前缀（协议对称）
pub const CREDENTIALS_MAGIC: &str = "BOTWORK_CREDENTIALS:";

/// 载荷长度限制 64KB（协议对称；发送侧决策点已校验，此为防御性二次校验）
pub const MAX_CREDENTIALS_PAYLOAD: usize = 64 * 1024;

/// 凭据消费错误（§7 错误映射：credentials_too_large / credentials_invalid；
/// 流中剥离失败报错终止该次连接，不降级——通道在 exec 时点已决策）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialsError {
    /// 载荷超过 64KB
    TooLarge,
    /// magic 行 / credentials 字段 JSON 解析失败
    Invalid,
}

/// MCP initialize 行消费结果（claude 长连接模式行级解析）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsumeOutcome {
    /// 非 initialize 行 / initialize 无 credentials：原样转发
    Passthrough,
    /// 消费到 credentials：驻留内存 + 转发剥离后的行（重新序列化字段顺序
    /// 可能变化，JSON 对象语义不变，claude CLI 解析不受影响）
    Consumed {
        credentials: HashMap<String, String>,
        forwarded_line: String,
    },
}

/// one-shot stdin 首行解析（协议对称实现）：
/// - 非 magic 行 → `Ok(None)`（env 回退通道无首行；调用方按正常载荷处理）
/// - magic 行 → 64KB 校验 + JSON 解析 → `Ok(Some(credentials))`
/// - 超限 / 解析失败 → `Err`（报错终止该次连接，不降级）
pub fn parse_credentials_first_line(
    line: &str,
) -> Result<Option<HashMap<String, String>>, CredentialsError> {
    let Some(payload) = line.strip_prefix(CREDENTIALS_MAGIC) else {
        return Ok(None);
    };
    if line.len() > MAX_CREDENTIALS_PAYLOAD {
        return Err(CredentialsError::TooLarge);
    }
    let credentials: HashMap<String, String> =
        serde_json::from_str(payload).map_err(|_| CredentialsError::Invalid)?;
    Ok(Some(credentials))
}

/// MCP initialize 行消费（claude 长连接模式行级解析，design v1.9 核实 1）：
/// - 非 JSON 行 / method != "initialize" / 无 params.credentials → 原样转发
///   （普通业务行解析失败不影响透传）
/// - 命中 initialize + credentials → 提取凭据（64KB 校验）→ 剥离 params 内
///   credentials 字段 → 重新序列化转发行
pub fn consume_initialize_credentials(line: &str) -> Result<ConsumeOutcome, CredentialsError> {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
        return Ok(ConsumeOutcome::Passthrough);
    };
    if value.get("method").and_then(|m| m.as_str()) != Some("initialize") {
        return Ok(ConsumeOutcome::Passthrough);
    }
    let Some(credentials_value) = value
        .get("params")
        .and_then(|p| p.get("credentials"))
        .cloned()
    else {
        return Ok(ConsumeOutcome::Passthrough);
    };
    if credentials_value.to_string().len() > MAX_CREDENTIALS_PAYLOAD {
        return Err(CredentialsError::TooLarge);
    }
    let credentials: HashMap<String, String> =
        serde_json::from_value(credentials_value).map_err(|_| CredentialsError::Invalid)?;
    if let Some(params) = value.get_mut("params").and_then(|p| p.as_object_mut()) {
        params.remove("credentials");
    }
    let forwarded_line =
        serde_json::to_string(&value).map_err(|_| CredentialsError::Invalid)?;
    Ok(ConsumeOutcome::Consumed {
        credentials,
        forwarded_line,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 与 bot-work-windows 侧 credential_channel::credentials_first_line 同构
    /// （magic + serde_json::Value 序列化），往返验证协议对称性。
    fn first_line_from(creds: &[(&str, &str)]) -> String {
        let map: HashMap<String, String> = creds
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let value = serde_json::to_value(&map).unwrap();
        format!("{CREDENTIALS_MAGIC}{value}")
    }

    #[test]
    fn test_first_line_roundtrip() {
        let line = first_line_from(&[
            ("OPENAI_API_KEY", "sk-test"),
            (
                "BOTWORK_PROVIDERS",
                r#"[{"baseUrl":"https://a","apiKey":"k"}]"#,
            ),
        ]);
        assert!(line.starts_with("BOTWORK_CREDENTIALS:{"));
        let parsed = parse_credentials_first_line(&line).unwrap().unwrap();
        assert_eq!(parsed.get("OPENAI_API_KEY").unwrap(), "sk-test");
        assert_eq!(
            parsed.get("BOTWORK_PROVIDERS").unwrap(),
            r#"[{"baseUrl":"https://a","apiKey":"k"}]"#
        );
    }

    #[test]
    fn test_first_line_non_magic_and_invalid() {
        assert!(parse_credentials_first_line("{\"jsonrpc\":\"2.0\"}").unwrap().is_none());
        assert!(parse_credentials_first_line("").unwrap().is_none());
        assert_eq!(
            parse_credentials_first_line("BOTWORK_CREDENTIALS:{not json}"),
            Err(CredentialsError::Invalid)
        );
    }

    #[test]
    fn test_first_line_too_large() {
        let big = "x".repeat(MAX_CREDENTIALS_PAYLOAD);
        let line = format!("{CREDENTIALS_MAGIC}{{\"OPENAI_API_KEY\":\"{big}\"}}");
        assert!(line.len() > MAX_CREDENTIALS_PAYLOAD);
        assert_eq!(
            parse_credentials_first_line(&line),
            Err(CredentialsError::TooLarge)
        );
    }

    #[test]
    fn test_consume_initialize_extracts_and_strips() {
        let line = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"bot-work-windows","version":"1.0"},"credentials":{"OPENAI_API_KEY":"sk-1","AGNES_API_KEY":"sk-2"}}}"#;
        match consume_initialize_credentials(line).unwrap() {
            ConsumeOutcome::Consumed {
                credentials,
                forwarded_line,
            } => {
                assert_eq!(credentials.get("OPENAI_API_KEY").unwrap(), "sk-1");
                assert_eq!(credentials.get("AGNES_API_KEY").unwrap(), "sk-2");
                let v: serde_json::Value = serde_json::from_str(&forwarded_line).unwrap();
                let params = v.get("params").unwrap();
                assert!(params.get("credentials").is_none(), "剥离后不含 credentials");
                assert_eq!(
                    params.get("protocolVersion").unwrap(),
                    "2025-06-18",
                    "其余 params 字段保留"
                );
                assert_eq!(v.get("method").unwrap(), "initialize");
                assert_eq!(v.get("id").unwrap(), 1);
            }
            other => panic!("expected Consumed, got {other:?}"),
        }
    }

    #[test]
    fn test_consume_passthrough_branches() {
        // initialize 无 credentials → 原样转发
        let line = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#;
        assert_eq!(
            consume_initialize_credentials(line).unwrap(),
            ConsumeOutcome::Passthrough
        );
        // 非 initialize 业务行 → 原样转发
        let line = r#"{"jsonrpc":"2.0","method":"tools/list","id":2}"#;
        assert_eq!(
            consume_initialize_credentials(line).unwrap(),
            ConsumeOutcome::Passthrough
        );
        // 非 JSON 行（噪声）→ 原样转发
        assert_eq!(
            consume_initialize_credentials("hello world").unwrap(),
            ConsumeOutcome::Passthrough
        );
    }

    #[test]
    fn test_consume_initialize_too_large() {
        let big = "x".repeat(MAX_CREDENTIALS_PAYLOAD);
        let line = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"credentials":{{"OPENAI_API_KEY":"{big}"}}}}}}"#
        );
        assert_eq!(
            consume_initialize_credentials(&line),
            Err(CredentialsError::TooLarge)
        );
    }

    #[test]
    fn test_consume_initialize_invalid_credentials_shape() {
        // credentials 非 string→string 映射 → Invalid（报错终止，不透传带凭据行）
        let line = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"credentials":{"OPENAI_API_KEY":123}}}"#;
        assert_eq!(
            consume_initialize_credentials(line),
            Err(CredentialsError::Invalid)
        );
    }
}