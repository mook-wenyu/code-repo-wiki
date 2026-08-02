# API 参考

## src

- `User` — 用户实体 — src\auth.rs:2
- `Role` — 用户角色枚举 — src\auth.rs:8
- `Authenticator` — 认证 trait — src\auth.rs:14
- `pub fn authenticate(username: &str, password: &str) -> Option<User>` — 验证用户名和密码，返回 User — src\auth.rs:19
- `pub fn generate_token(user: &User) -> String` — 生成 JWT token — src\auth.rs:31
- `auth` —  — src\main.rs:1
- `storage` —  — src\main.rs:2
- `fn main()` — 应用主入口 — src\main.rs:5

## src::storage

- `SessionStore` — 会话存储结构体 — src\storage.rs:4
- `impl SessionStore {` —  — src\storage.rs:8
- `pub fn new() -> Self` — 创建空存储 — src\storage.rs:10
- `pub fn insert(&mut self, key: String, value: String)` — 插入会话 — src\storage.rs:15
- `pub fn save_session(username: &str)` — 保存用户会话（简化版） — src\storage.rs:21
- `pub fn load_session(username: &str) -> Option<String>` — 加载用户会话 — src\storage.rs:26
