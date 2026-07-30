mod auth;
mod storage;

/// 应用主入口
fn main() {
    let user = auth::authenticate("admin", "secret");
    if let Some(u) = user {
        storage::save_session(&u.name);
    }
}
