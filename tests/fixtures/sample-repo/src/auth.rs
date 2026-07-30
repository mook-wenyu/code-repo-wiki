/// 用户实体
pub struct User {
    pub name: String,
    pub role: Role,
}

/// 用户角色枚举
pub enum Role {
    Admin,
    Viewer,
}

/// 认证 trait
pub trait Authenticator {
    fn verify(&self, token: &str) -> bool;
}

/// 验证用户名和密码，返回 User
pub fn authenticate(username: &str, password: &str) -> Option<User> {
    if username == "admin" && password == "secret" {
        Some(User {
            name: username.to_string(),
            role: Role::Admin,
        })
    } else {
        None
    }
}

/// 生成 JWT token
pub fn generate_token(user: &User) -> String {
    format!("token-{}-admin", user.name)
}
