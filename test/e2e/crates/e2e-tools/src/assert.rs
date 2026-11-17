//! 断言原语 + 证据收集。每条断言落一行证据，报告直接引用；失败返回带 divergence 的错误。

use std::fmt::Debug;

#[derive(Default)]
pub struct Evidence {
    pub lines: Vec<String>,
}

impl Evidence {
    pub fn note(&mut self, msg: impl Into<String>) {
        self.lines.push(msg.into());
    }

    /// 精确相等断言
    pub fn expect_eq<T: Debug + PartialEq>(
        &mut self,
        what: &str,
        actual: T,
        expected: T,
    ) -> anyhow::Result<()> {
        if actual == expected {
            self.lines
                .push(format!("{what}: {actual:?} == expected {expected:?}"));
            Ok(())
        } else {
            anyhow::bail!("{what}: DIVERGENCE actual={actual:?} expected={expected:?}")
        }
    }

    pub fn expect_true(&mut self, what: &str, cond: bool) -> anyhow::Result<()> {
        if cond {
            self.lines.push(format!("{what}: true"));
            Ok(())
        } else {
            anyhow::bail!("{what}: DIVERGENCE expected true, got false")
        }
    }

    pub fn expect_ge<T: Debug + PartialOrd>(
        &mut self,
        what: &str,
        actual: T,
        min: T,
    ) -> anyhow::Result<()> {
        if actual >= min {
            self.lines
                .push(format!("{what}: {actual:?} >= min {min:?}"));
            Ok(())
        } else {
            anyhow::bail!("{what}: DIVERGENCE actual={actual:?} < min {min:?}")
        }
    }
}
