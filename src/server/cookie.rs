//! 会话 cookie：openid 的签发与解析（`docs/DESIGN.md` §1 / §6）。
//!
//! 零持久化：openid 只存在于浏览器 cookie（2h 过期），服务端不落盘。
//! cookie 不设 `HttpOnly`，以便前端在收到 `SessionExpired` 后自行清除。

use axum::http::{HeaderMap, HeaderValue, header};

/// 会话 cookie 名。
pub const COOKIE_NAME: &str = "openid";
/// 会话有效期（秒）：2 小时。
pub const COOKIE_MAX_AGE_SECS: u64 = 7_200;

/// 构造 `Set-Cookie` 头值；`openid` 含非法字符时返回 `None`。
#[must_use]
pub fn issue(openid: &str) -> Option<HeaderValue> {
    let openid = validate(openid)?;
    let value =
        format!("{COOKIE_NAME}={openid}; Path=/; Max-Age={COOKIE_MAX_AGE_SECS}; SameSite=Lax");
    HeaderValue::from_str(&value).ok()
}

/// 校验并归一化 openid：去首尾空白，拒绝空串、控制字符与 Cookie 分隔符。
#[must_use]
pub fn validate(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || !trimmed.bytes().all(is_safe_byte) {
        return None;
    }
    Some(String::from(trimmed))
}

/// 从请求头解析 cookie 中的 openid；缺失或非法时返回 `None`。
#[must_use]
pub fn parse(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';').find_map(|part| {
        let (name, value) = part.split_once('=')?;
        if name.trim() != COOKIE_NAME {
            return None;
        }
        validate(value)
    })
}

/// 仅接受可打印 ASCII，排除 `;`（Cookie 分隔符）与引号、反斜杠。
fn is_safe_byte(byte: u8) -> bool {
    matches!(byte, 0x21..=0x7e) && !matches!(byte, b';' | b'"' | b'\\')
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

    fn cookie_header(value: &str) -> TestResult<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, HeaderValue::from_str(value)?);
        Ok(headers)
    }

    #[test]
    fn issued_cookie_carries_two_hour_max_age() -> TestResult {
        let value = issue("openid-under-test").ok_or("未签发 cookie")?;
        assert_eq!(
            value.to_str()?,
            "openid=openid-under-test; Path=/; Max-Age=7200; SameSite=Lax"
        );
        Ok(())
    }

    #[test]
    fn parse_reads_openid_from_cookie_header() -> TestResult {
        let headers = cookie_header("theme=dark; openid=openid-under-test; other=1")?;
        assert_eq!(parse(&headers).as_deref(), Some("openid-under-test"));
        Ok(())
    }

    #[test]
    fn parse_returns_none_when_absent() -> TestResult {
        assert_eq!(parse(&HeaderMap::new()), None);
        assert_eq!(parse(&cookie_header("theme=dark")?), None);
        Ok(())
    }

    #[test]
    fn unsafe_openid_is_rejected() -> TestResult {
        assert_eq!(issue("bad;value"), None);
        assert_eq!(issue(""), None);
        assert_eq!(issue("   "), None);
        // `;` 是 Cookie 分隔符，先被拆分；含空格的值仍应被拒绝。
        assert_eq!(parse(&cookie_header("openid=bad value")?), None);
        Ok(())
    }
}
