use std::collections::HashMap;

/// 会话存储结构体
pub struct SessionStore {
    sessions: HashMap<String, String>,
}

impl SessionStore {
    /// 创建空存储
    pub fn new() -> Self {
        Self { sessions: HashMap::new() }
    }

    /// 插入会话
    pub fn insert(&mut self, key: String, value: String) {
        self.sessions.insert(key, value);
    }
}

/// 保存用户会话（简化版）
pub fn save_session(username: &str) {
    let _path = format!("/tmp/session_{}.json", username);
}

/// 加载用户会话
pub fn load_session(username: &str) -> Option<String> {
    let _path = format!("/tmp/session_{}.json", username);
    None
}
