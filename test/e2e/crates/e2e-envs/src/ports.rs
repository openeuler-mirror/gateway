//! 随机端口分配（探测式：bind 成功即视为可用；冲突概率低，网关 boot 失败会显式报错）

pub fn alloc_tcp() -> u16 {
    let mut tries = 0;
    loop {
        let port = 30000 + rand::random::<u16>() % 8000;
        if port_free(port) {
            return port;
        }
        tries += 1;
        if tries > 50 {
            return port; // 探测不到就交出去，让 spawn 显式失败
        }
    }
}

pub fn port_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

pub fn alloc_run_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        % 1_000_000;
    format!("r{ms:06x}{}", rand::random::<u16>())
}
