//! 项目根目录（消除进程级 current_dir 依赖的注入载体）
//!
//! 背景：全仓库多处通过 `std::env::current_dir()` 推导项目根（扫描根、
//! wiki_plan.yaml 定位、git 仓库定位、指纹键前缀），导致三方面问题：
//! - 不可测试：测试进程的 cwd 是测试运行目录，与被测代码假设的
//!   "项目根"不一致，根路径相关断言只能在真实仓库里跑；
//! - 不可常驻：watch 长驻进程 / 服务化场景中 cwd 可被外部改变，
//!   依赖 cwd 的路径推导会静默漂移（扫描范围、git 仓库定位全错）；
//! - 不可并行：多实例（并行子代理、多输出目录）共享同一 cwd 时
//!   相互干扰。
//!
//! 方案：CLI 显式 --root 参数（main.rs 由主代理接入）→ 本类型全链路
//! 注入。各模块公开入口保留原签名并委托 [`ProjectRoot::from_cwd`]
//! （兼容现有调用点，保证 lib.rs/main.rs 编译不冲突），主代理合入时
//! 把调用点切换为 *_at 变体并移除委托，届时 current_dir 依赖归零。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// 项目根目录（仅承载路径，不校验存在性）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRoot {
    path: PathBuf,
}

impl ProjectRoot {
    /// 默认根：当前工作目录
    ///
    /// 仅供兼容委托使用；主代理合入 --root 注入后此方法不再被调用，
    /// 届时删除。
    pub fn from_cwd() -> Result<Self> {
        let path = std::env::current_dir().context("获取当前工作目录失败")?;
        Ok(Self::new(path))
    }

    /// 显式指定项目根（CLI --root 注入入口）
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// 项目根路径
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 项目根下的相对路径
    pub fn join(&self, rel: &Path) -> PathBuf {
        self.path.join(rel)
    }
}
