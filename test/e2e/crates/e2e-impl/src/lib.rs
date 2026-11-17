//! L3 用例实现：每条台账条目一个测试函数 + 中心注册表。
//! 红线：新用例先入台账（cases/），再在此登记；ID 与台账一一对应（bind-check 校验）。

pub mod broadcast;
pub mod metering;

use e2e_envs::TestEnv;
use futures::future::BoxFuture;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvGroup {
    GBase,
    GFresh,
    GFault,
}

/// 用例函数：输入就绪环境，输出证据行；Err = 失败（带 divergence）
pub type CaseFn = for<'a> fn(&'a TestEnv) -> BoxFuture<'a, anyhow::Result<Vec<String>>>;

pub struct CaseEntry {
    /// 台账 ID（MI-xxx），与 cases/ frontmatter 一一对应
    pub id: &'static str,
    pub title: &'static str,
    pub env_group: EnvGroup,
    pub severity: &'static str,
    /// quota 判定口径（台账 env.decision_base_shared，缺省 false）。
    /// 不同取值需要不同环境——runner 按 (env_group, decision_base_shared) 分叉起环境。
    pub decision_base_shared: bool,
    pub run: CaseFn,
}

/// 中心注册表：全部已实现用例（bind-check 与 cases/ 台账双向校验）
pub fn all_cases() -> Vec<&'static CaseEntry> {
    let mut all: Vec<&'static CaseEntry> = Vec::new();
    all.extend(metering::ALL);
    all.extend(broadcast::ALL);
    all
}
