//! Parsing of a pod's `sshConnection` / `ip` fields into user, host and port.
//! The host matters: marketplace pods are often reached through a NAT address
//! and port that differ from `ip`.

use serde_json::Value;

/// What `sshConnection` says about how to log in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SshConnection {
    pub user: Option<String>,
    pub host: Option<String>,
    /// 22 when not stated.
    pub port: u16,
}

/// Parse `sshConnection`, which the API returns as a string or a list of
/// strings/nulls. Accepted shapes: `"user@host -p 2222 [-o ...]"`,
/// `"user@host:2222"`, `"user@host"`, `["user@host", "-p", "2222"]`. `None`
/// when it holds no tokens (not ready yet).
pub fn parse_ssh_connection(value: &Value) -> Option<SshConnection> {
    let tokens: Vec<String> = match value {
        Value::String(s) => s.split_whitespace().map(str::to_string).collect(),
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .flat_map(|s| s.split_whitespace().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    if tokens.is_empty() {
        return None;
    }

    let mut conn = SshConnection {
        port: 22,
        ..Default::default()
    };
    if let Some(target) = tokens.iter().find(|t| t.contains('@')) {
        let (user, host_part) = target.split_once('@').expect("contains '@'");
        if !user.is_empty() {
            conn.user = Some(user.to_string());
        }
        let host = match host_part.rsplit_once(':') {
            Some((host, port)) => match port.parse() {
                Ok(port) => {
                    conn.port = port;
                    host
                }
                Err(_) => host_part,
            },
            None => host_part,
        };
        if !host.is_empty() {
            conn.host = Some(host.to_string());
        }
    }
    // An explicit `-p <port>` wins over `host:port`.
    if let Some(i) = tokens.iter().position(|t| t == "-p") {
        if let Some(port) = tokens.get(i + 1).and_then(|p| p.parse().ok()) {
            conn.port = port;
        }
    }
    Some(conn)
}

/// First usable address from `ip` (a string, or a list of strings/nulls).
pub fn first_ip(value: &Value) -> Option<String> {
    match value {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .find(|s| !s.is_empty())
            .map(str::to_string),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn parse(v: Value) -> SshConnection {
        parse_ssh_connection(&v).unwrap()
    }

    #[test]
    fn port_flag_with_extra_options() {
        let c = parse(json!("ubuntu@1.2.3.4 -p 2222 -o StrictHostKeyChecking=no"));
        assert_eq!(c.user.as_deref(), Some("ubuntu"));
        assert_eq!(c.host.as_deref(), Some("1.2.3.4"));
        assert_eq!(c.port, 2222);
    }

    #[test]
    fn host_port_and_default_port() {
        let c = parse(json!("root@5.6.7.8:40022"));
        assert_eq!(
            (c.user.as_deref(), c.host.as_deref(), c.port),
            (Some("root"), Some("5.6.7.8"), 40022)
        );
        let c = parse(json!("ubuntu@1.2.3.4"));
        assert_eq!(c.port, 22);
    }

    #[test]
    fn list_shapes_and_flag_precedence() {
        let c = parse(json!(["ubuntu@1.2.3.4", "-p", "2201"]));
        assert_eq!((c.host.as_deref(), c.port), (Some("1.2.3.4"), 2201));
        let c = parse(json!(["root@9.9.9.9:2000 -p 3000", null]));
        assert_eq!(c.port, 3000);
    }

    #[test]
    fn empty_or_null_is_not_ready() {
        assert!(parse_ssh_connection(&json!(null)).is_none());
        assert!(parse_ssh_connection(&json!("")).is_none());
        assert!(parse_ssh_connection(&json!([null])).is_none());
    }

    #[test]
    fn tokens_without_user_host_keep_defaults() {
        let c = parse(json!("-p 2222"));
        assert_eq!((c.user, c.host, c.port), (None, None, 2222));
    }

    #[test]
    fn first_ip_handles_string_and_list() {
        assert_eq!(first_ip(&json!("1.2.3.4")).as_deref(), Some("1.2.3.4"));
        assert_eq!(
            first_ip(&json!([null, "5.6.7.8"])).as_deref(),
            Some("5.6.7.8")
        );
        assert!(first_ip(&json!(null)).is_none());
        assert!(first_ip(&json!("")).is_none());
    }
}
