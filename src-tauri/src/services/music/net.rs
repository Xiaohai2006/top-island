//! 音乐域共享的 HTTP 取回：连接池 + JSON 解析 + URL 编码。
//! 都是阻塞 ureq 调用，调用方负责放到后台线程（resolver 的抓取线程）。

use std::sync::OnceLock;
use std::time::Duration;

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36";
const TIMEOUT: Duration = Duration::from_secs(8);

/// 共享连接池，免得每个请求重做 TLS 握手
fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            .build()
            .into()
    })
}

pub fn fetch_json(url: &str, referer: Option<&str>) -> Option<serde_json::Value> {
    let mut req = agent().get(url).header("User-Agent", UA);
    if let Some(r) = referer {
        req = req.header("Referer", r);
    }
    let mut resp = match req.call() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[music:http] 请求失败 ({url}): {e}");
            return None;
        }
    };
    match resp.body_mut().read_json::<serde_json::Value>() {
        Ok(v) => Some(v),
        Err(e) => {
            eprintln!("[music:http] 响应不是 JSON ({url}): {e}");
            None
        }
    }
}

/// encodeURIComponent 等价：保留 RFC 3986 非保留字符 + ! ~ * ' ( )
pub fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_encode_keeps_unreserved_and_percent_encodes_utf8() {
        assert_eq!(url_encode("a b"), "a%20b", "空格应编码为 %20");
        assert_eq!(url_encode("歌"), "%E6%AD%8C", "中文应按 UTF-8 逐字节编码，错了搜索词会乱码");
        assert_eq!(url_encode("a-b_c.!~*'()"), "a-b_c.!~*'()", "非保留字符不应编码");
    }
}
