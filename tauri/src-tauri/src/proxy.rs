//! 代理环境变量构造与 Windows 系统代理读取。
//!
//! 关键点：Node 的 fetch 默认忽略 HTTP(S)_PROXY 环境变量（需 NODE_USE_ENV_PROXY=1），
//! 而 npm 完全不读这些变量（需 npm_config_proxy / npm_config_https_proxy），
//! 两套开关都必须注入，缺一不可。

const NO_PROXY_DEFAULT: &str = "127.0.0.1,localhost,::1";

pub(crate) fn get_system_proxy() -> (bool, String) {
    use windows_registry::*;
    match CURRENT_USER.open(r"Software\Microsoft\Windows\CurrentVersion\Internet Settings") {
        Ok(key) => {
            let enable = key.get_u32("ProxyEnable").unwrap_or(0) != 0;
            let server = key.get_string("ProxyServer").unwrap_or_default();
            (enable, server)
        }
        Err(_) => (false, String::new()),
    }
}

#[tauri::command]
pub(crate) fn get_system_proxy_cmd() -> (bool, String) {
    get_system_proxy()
}

/// 解析 Windows ProxyServer 值：
/// - `http=…;https=…` 分协议形式 → (http, https)
/// - 单一地址形式 → 同一地址同时用于 http 与 https
pub(crate) fn parse_proxy_server(server: &str) -> (Option<String>, Option<String>) {
    if server.contains('=') {
        let mut http = None;
        let mut https = None;
        for part in server.split(';') {
            let part = part.trim();
            if let Some(eq) = part.find('=') {
                let (scheme, addr) = part.split_at(eq);
                let addr = addr[1..].trim();
                match scheme.trim().to_lowercase().as_str() {
                    "http" => http = Some(addr.to_string()),
                    "https" => https = Some(addr.to_string()),
                    _ => {}
                }
            }
        }
        (http, https)
    } else {
        let s = server.trim().to_string();
        if s.is_empty() {
            (None, None)
        } else {
            (Some(s.clone()), Some(s))
        }
    }
}

/// 补全代理地址的 scheme（缺失时按 http 处理，Node 与 npm 都要求带 scheme）
pub(crate) fn normalize_proxy_url(addr: &str) -> Option<String> {
    let a = addr.trim();
    if a.is_empty() {
        return None;
    }
    if a.contains("://") {
        Some(a.to_string())
    } else {
        Some(format!("http://{}", a))
    }
}

/// 为 DSH（node）子进程构造代理环境变量。
/// NODE_USE_ENV_PROXY=1 是关键开关：Node 的 fetch 默认忽略环境变量代理。
pub(crate) fn build_proxy_env(server: &str) -> Vec<(String, String)> {
    let (http, https) = parse_proxy_server(server);
    if http.is_none() && https.is_none() {
        return Vec::new();
    }
    let mut env: Vec<(String, String)> = Vec::new();
    if let Some(h) = &http {
        if let Some(u) = normalize_proxy_url(h) {
            env.push(("HTTP_PROXY".into(), u.clone()));
            env.push(("http_proxy".into(), u));
        }
    }
    if let Some(h) = &https {
        if let Some(u) = normalize_proxy_url(h) {
            env.push(("HTTPS_PROXY".into(), u.clone()));
            env.push(("https_proxy".into(), u));
        }
    }
    let all = normalize_proxy_url(http.as_deref().or(https.as_deref()).unwrap_or(""));
    if let Some(a) = all {
        env.push(("ALL_PROXY".into(), a));
    }
    env.push(("NO_PROXY".into(), NO_PROXY_DEFAULT.into()));
    env.push(("no_proxy".into(), NO_PROXY_DEFAULT.into()));
    env.push(("NODE_USE_ENV_PROXY".into(), "1".into()));
    env
}

/// npm 专用的代理环境：npm 不读 HTTP_PROXY 环境变量（与 node 不同），
/// 必须用 npm_config_proxy / npm_config_https_proxy 显式指定，否则开了
/// 代理的机器上 npm install / npm view 仍然直连导致失败。
pub(crate) fn npm_proxy_env(proxy_on: bool, proxy_addr: &str) -> Vec<(String, String)> {
    let mut env = proxy_env_or_none(proxy_on, proxy_addr);
    if !env.is_empty() {
        let (http, https) = parse_proxy_server(proxy_addr);
        if let Some(h) = normalize_proxy_url(http.as_deref().or(https.as_deref()).unwrap_or("")) {
            env.push(("npm_config_proxy".into(), h.clone()));
            env.push(("npm_config_https_proxy".into(), h));
        }
    }
    env
}

/// 代理开关关闭时返回空列表（不注入任何变量）
pub(crate) fn proxy_env_or_none(proxy_on: bool, proxy_addr: &str) -> Vec<(String, String)> {
    if !proxy_on {
        return Vec::new();
    }
    build_proxy_env(proxy_addr)
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use super::*;

    #[test]
    fn parses_per_protocol_and_single_addr_forms() {
        let (http, https) = parse_proxy_server("http=127.0.0.1:8080;https=127.0.0.1:8443");
        assert_eq!(http.as_deref(), Some("127.0.0.1:8080"));
        assert_eq!(https.as_deref(), Some("127.0.0.1:8443"));
        let (http, https) = parse_proxy_server(" 127.0.0.1:7890 ");
        assert_eq!(http.as_deref(), Some("127.0.0.1:7890"));
        assert_eq!(https.as_deref(), Some("127.0.0.1:7890"));
        assert_eq!(parse_proxy_server(""), (None, None));
        // 未知协议被忽略
        let (http, https) = parse_proxy_server("ftp=1.2.3.4:21");
        assert_eq!(http, None);
        assert_eq!(https, None);
    }

    #[test]
    fn normalizes_scheme() {
        assert_eq!(
            normalize_proxy_url("127.0.0.1:7890").as_deref(),
            Some("http://127.0.0.1:7890")
        );
        assert_eq!(
            normalize_proxy_url("socks5://127.0.0.1:7891").as_deref(),
            Some("socks5://127.0.0.1:7891")
        );
        assert_eq!(normalize_proxy_url("   "), None);
    }

    #[test]
    fn builds_node_env_with_critical_switch() {
        let env: HashMap<String, String> = build_proxy_env("http=1.2.3.4:8080;https=1.2.3.4:8080")
            .into_iter()
            .collect();
        assert_eq!(
            env.get("HTTP_PROXY").map(String::as_str),
            Some("http://1.2.3.4:8080")
        );
        assert_eq!(
            env.get("HTTPS_PROXY").map(String::as_str),
            Some("http://1.2.3.4:8080")
        );
        assert_eq!(
            env.get("ALL_PROXY").map(String::as_str),
            Some("http://1.2.3.4:8080")
        );
        assert_eq!(
            env.get("NO_PROXY").map(String::as_str),
            Some(NO_PROXY_DEFAULT)
        );
        // 小写变量同样注入（部分工具只认小写）
        assert!(env.contains_key("http_proxy"));
        assert!(env.contains_key("no_proxy"));
        // Node fetch 默认忽略环境变量代理，此开关是代理生效的关键
        assert_eq!(env.get("NODE_USE_ENV_PROXY").map(String::as_str), Some("1"));
    }

    #[test]
    fn npm_env_adds_npm_config_and_respects_switch() {
        let env: HashMap<String, String> =
            npm_proxy_env(true, "127.0.0.1:7890").into_iter().collect();
        // npm 不读 HTTP_PROXY，必须走 npm_config_*
        assert_eq!(
            env.get("npm_config_proxy").map(String::as_str),
            Some("http://127.0.0.1:7890")
        );
        assert_eq!(
            env.get("npm_config_https_proxy").map(String::as_str),
            Some("http://127.0.0.1:7890")
        );
        // 开关关闭：不注入任何变量
        assert!(npm_proxy_env(false, "127.0.0.1:7890").is_empty());
        assert!(npm_proxy_env(true, "").is_empty());
    }

    #[test]
    fn no_duplicate_keys() {
        let env = build_proxy_env("http=a:1;https=a:1");
        let keys: HashSet<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys.len(), env.len(), "环境变量键不得重复：{:?}", env);
    }
}
