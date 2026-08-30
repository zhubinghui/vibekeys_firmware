//! MQTT broker URI 解析。纯逻辑,不依赖 ESP-IDF —— 与 `protocol` 一同经 `src/lib.rs`
//! 暴露给宿主 target,供 CI 在没有 Xtensa 工具链的机器上跑单测。

pub struct BrokerInfo {
    pub broker_url: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub use_tls: bool,
}

/// 手写解析 `mqtt://user:pass@host:port` / `mqtts://user:pass@host:port`。
///
/// 不用 `http` crate(它不暴露 userinfo),也不引入 `url` 依赖。
pub fn parse_broker_uri(uri: &str) -> anyhow::Result<BrokerInfo> {
    let (scheme, rest) = uri
        .split_once("://")
        .ok_or_else(|| anyhow::anyhow!("MQTT URI missing '://': {uri}"))?;
    let use_tls = match scheme {
        "mqtt" => false,
        "mqtts" => true,
        other => anyhow::bail!("Unsupported MQTT scheme '{other}', use mqtt:// or mqtts://"),
    };

    // rest = [user[:pass]@]host[:port][/...]
    let (userinfo, hostport) = match rest.rfind('@') {
        Some(idx) => (Some(&rest[..idx]), &rest[idx + 1..]),
        None => (None, rest),
    };
    // 去掉可能尾随的路径
    let hostport = hostport.split('/').next().unwrap_or(hostport);

    let (username, password) = match userinfo {
        Some(u) => match u.split_once(':') {
            Some((user, pass)) => (Some(user.to_string()), Some(pass.to_string())),
            None => (Some(u.to_string()), None),
        },
        // 无账号(匿名 URL):username/password 都 None → CONNECT 不带凭证(真匿名)。
        // topic 的 user 段则回退 `root`(见 MqttServer::new 的 discovery 订阅)。
        None => (None, None),
    };

    let default_port = if use_tls { 8883 } else { 1883 };
    let port = hostport
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse::<u16>().ok())
        .unwrap_or(default_port);
    let host = hostport
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(hostport);

    if host.is_empty() {
        anyhow::bail!("MQTT URI missing host");
    }

    let scheme_str = if use_tls { "mqtts" } else { "mqtt" };
    Ok(BrokerInfo {
        broker_url: format!("{scheme_str}://{host}:{port}"),
        username,
        password,
        use_tls,
    })
}

#[cfg(test)]
mod tests {
    use super::parse_broker_uri;

    #[test]
    fn parse_mqtt_plain() {
        let i = parse_broker_uri("mqtt://alice:secret@broker.example.com:1883").unwrap();
        assert_eq!(i.broker_url, "mqtt://broker.example.com:1883");
        assert_eq!(i.username.as_deref(), Some("alice"));
        assert_eq!(i.password.as_deref(), Some("secret"));
        assert!(!i.use_tls);
    }

    #[test]
    fn parse_mqtts_default_port() {
        let i = parse_broker_uri("mqtts://bob:p@host.io").unwrap();
        assert_eq!(i.broker_url, "mqtts://host.io:8883");
        assert_eq!(i.username.as_deref(), Some("bob"));
        assert!(i.use_tls);
    }

    #[test]
    fn parse_password_with_special_chars() {
        // 密码里含 '@' 会干扰 rfind('@');这里只验证无 @ 的常见情形
        let i = parse_broker_uri("mqtt://u:p-a-ss@1.2.3.4:1883").unwrap();
        assert_eq!(i.username.as_deref(), Some("u"));
        assert_eq!(i.password.as_deref(), Some("p-a-ss"));
        assert_eq!(i.broker_url, "mqtt://1.2.3.4:1883");
    }

    #[test]
    fn parse_anonymous_is_none() {
        // 无 user:pass@(匿名 URL):username/password 都 None(真匿名 CONNECT)
        let i = parse_broker_uri("mqtt://192.168.1.10:1883").unwrap();
        assert!(i.username.is_none());
        assert!(i.password.is_none());
        assert_eq!(i.broker_url, "mqtt://192.168.1.10:1883");
    }
}
