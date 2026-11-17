//! 进程控制原语：带日志重定向的 spawn、kill -9 / SIGSTOP / SIGCONT / SIGHUP。

use std::path::Path;
use std::process::Stdio;

use tokio::io::AsyncWriteExt;

pub struct ManagedChild {
    pub name: String,
    pub pid: i32,
    child: tokio::process::Child,
    log_file: std::path::PathBuf,
}

impl ManagedChild {
    /// spawn 并把 stdout/stderr 重定向到 log_file（两路合并）
    pub fn spawn(
        name: &str,
        bin: &str,
        args: &[&str],
        log_file: &Path,
        envs: &[(String, String)],
    ) -> anyhow::Result<Self> {
        if let Some(parent) = log_file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let log_out = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_file)?;
        let log_err = log_out.try_clone()?;
        let mut cmd = tokio::process::Command::new(bin);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log_out))
            .stderr(Stdio::from(log_err))
            // 独立进程组：SIGSTOP/SIGHUP 不会误伤 runner 自身；teardown 可 killpg 整组
            .process_group(0)
            .kill_on_drop(true);
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let child = cmd.spawn()?;
        let pid = child
            .id()
            .ok_or_else(|| anyhow::anyhow!("spawn {name}: no pid"))? as i32;
        Ok(Self {
            name: name.to_string(),
            pid,
            child,
            log_file: log_file.to_path_buf(),
        })
    }

    pub fn log_path(&self) -> &Path {
        &self.log_file
    }

    pub fn tail_log(&self, n: usize) -> Vec<String> {
        tail_file(&self.log_file, n)
    }

    /// kill -9 整个进程组
    pub fn kill9(&mut self) -> anyhow::Result<()> {
        killpg(self.pid)
    }

    pub fn sigstop(&self) -> anyhow::Result<()> {
        signal_group(self.pid, libc::SIGSTOP)
    }

    pub fn sigcont(&self) -> anyhow::Result<()> {
        signal_group(self.pid, libc::SIGCONT)
    }

    pub fn sighup(&self) -> anyhow::Result<()> {
        signal_group(self.pid, libc::SIGHUP)
    }

    /// 已退出则返回 Some(exit_code)
    pub async fn try_wait(&mut self) -> anyhow::Result<Option<i32>> {
        Ok(self.child.try_wait()?.map(|st| st.code().unwrap_or(-1)))
    }

    /// 等待退出，返回 exit code
    pub async fn wait(&mut self) -> anyhow::Result<i32> {
        let st = self.child.wait().await?;
        Ok(st.code().unwrap_or(-1))
    }
}

pub fn killpg(pid: i32) -> anyhow::Result<()> {
    // process_group(0) 使子 pid == pgid
    let r = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    if r != 0 {
        let r2 = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        if r2 != 0 {
            anyhow::bail!("kill9 {pid}: errno={}", std::io::Error::last_os_error());
        }
    }
    Ok(())
}

pub fn signal_group(pid: i32, sig: i32) -> anyhow::Result<()> {
    let r = unsafe { libc::kill(pid as i32, sig) };
    if r != 0 {
        anyhow::bail!(
            "signal({pid}, {sig}): {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

pub fn tail_file(path: &Path, n: usize) -> Vec<String> {
    let data = std::fs::read_to_string(path).unwrap_or_default();
    let mut lines: Vec<String> = data
        .lines()
        .map(|s| s.trim_end().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if lines.len() > n {
        lines.drain(..lines.len() - n);
    }
    lines
}

/// 把一行追加写进日志文件（postgres 直接 spawn 时用不到，postgres 走 pg_ctl -l）
pub async fn append_log(path: &Path, line: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    f.write_all(format!("{line}\n").as_bytes()).await?;
    Ok(())
}
