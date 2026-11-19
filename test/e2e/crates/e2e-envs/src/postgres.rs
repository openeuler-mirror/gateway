//! 临时 Postgres 实例：initdb → pg_ctl start → 就绪探测；postmaster 可 kill / SIGSTOP / SIGCONT（故障注入）。

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context};

pub const PG_USER: &str = "e2e";
pub const PG_DB: &str = "e2e";

pub struct PostgresInstance {
    pub data_dir: PathBuf,
    pub socket_dir: PathBuf,
    pub port: u16,
    pub dsn: String,
    pub bin_dir: PathBuf,
}

/// 找 postgres 服务端工具目录（initdb/pg_ctl/psql 同目录）。
/// 顺序：E2E_PG_BIN_DIR 环境变量 → PATH → 常见安装位置 glob（版本取最大）。
pub fn find_pg_bin_dir() -> anyhow::Result<PathBuf> {
    if let Ok(dir) = std::env::var("E2E_PG_BIN_DIR") {
        let p = PathBuf::from(&dir);
        if p.join("initdb").exists() {
            return Ok(p);
        }
        bail!("E2E_PG_BIN_DIR={dir} 下没有 initdb");
    }
    if let Ok(which) = Command::new("which")
        .arg("initdb")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    {
        if !which.is_empty() {
            return Ok(PathBuf::from(which)
                .parent()
                .ok_or_else(|| anyhow::anyhow!("initdb path has no parent"))?
                .to_path_buf());
        }
    }
    let candidates: [(&str, fn(Vec<PathBuf>) -> Option<PathBuf>); 3] = [
        ("/usr/lib/postgresql", vec_glob_max),
        ("/opt/homebrew/opt", brew_glob),
        ("/usr/local/opt", brew_glob),
    ];
    for (root, globber) in candidates {
        let root = Path::new(root);
        if root.exists() {
            let hits: Vec<PathBuf> = std::fs::read_dir(root)?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.join("bin").join("initdb").exists())
                .map(|p| p.join("bin"))
                .collect();
            if let Some(p) = globber(hits) {
                return Ok(p);
            }
        }
    }
    bail!(
        "找不到 initdb/pg_ctl/psql。Debian/Ubuntu: apt-get install -y postgresql postgresql-client; \
         macOS: brew install postgresql@18; 或设 E2E_PG_BIN_DIR 指向 bin 目录"
    )
}

fn vec_glob_max(hits: Vec<PathBuf>) -> Option<PathBuf> {
    // /usr/lib/postgresql/<ver>/bin 取最大版本
    hits.into_iter().max()
}

fn brew_glob(hits: Vec<PathBuf>) -> Option<PathBuf> {
    // /opt/homebrew/opt/postgresql@18/bin 取最大 @ver；postgres formula 也认
    hits.into_iter()
        .filter(|p| {
            let name = p.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
            name == "postgres" || name.starts_with("postgresql@")
        })
        .max_by_key(|p| {
            let s = p.to_string_lossy().to_string();
            s.rsplit('@')
                .next()
                .and_then(|v| v.trim_end_matches("/bin").parse::<u32>().ok())
                .unwrap_or(0)
        })
}

impl PostgresInstance {
    /// initdb（不启动）。目录约定：run_root/pg/{name}
    pub fn init(run_root: &Path) -> anyhow::Result<Self> {
        let bin_dir = find_pg_bin_dir()?;
        let initdb = bin_dir.join("initdb");
        let data_dir = run_root.join("pg").join("data");
        let socket_dir = run_root.join("pg").join("sock");
        if data_dir.exists() {
            std::fs::remove_dir_all(&data_dir).context("清理残留 pg data")?;
        }
        std::fs::create_dir_all(&socket_dir)?;
        set_mode_700(&socket_dir)?;

        let st = Command::new(&initdb)
            .arg("-D").arg(&data_dir)
            .arg("--no-locale")
            .arg("-E").arg("UTF8")
            .arg("-U").arg(PG_USER)
            .arg("--auth=trust")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .context("执行 initdb")?;
        if !st.success() {
            // 失败重跑一次带输出，方便排障
            let out = Command::new(&initdb)
                .arg("-D").arg(&data_dir)
                .arg("--no-locale")
                .arg("-E").arg("UTF8")
                .arg("-U").arg(PG_USER)
                .arg("--auth=trust")
                .output()
                .context("initdb 重试")?;
            bail!("initdb 失败: {}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        }

        let port = crate::ports::alloc_tcp();
        let dsn = format!("postgres://{PG_USER}@127.0.0.1:{port}/{PG_DB}");
        Ok(Self {
            data_dir,
            socket_dir,
            port,
            dsn,
            bin_dir,
        })
    }

    /// pg_ctl start + 就绪探测（psql SELECT 1，10s 超时）
    pub fn start(&self) -> anyhow::Result<()> {
        let pg_ctl = self.bin_dir.join("pg_ctl");
        let server_log = self.data_dir.parent().unwrap().join("server.log");
        let opts = format!("-p {} -k {} -h 127.0.0.1", self.port, self.socket_dir.display());
        let st = Command::new(&pg_ctl)
            .arg("start")
            .arg("-D").arg(&self.data_dir)
            .arg("-l").arg(&server_log)
            .arg("-o").arg(&opts)
            .arg("-w").arg("-t").arg("10")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .context("pg_ctl start")?;
        if !st.success() {
            let log = e2e_tools::proc::tail_file(&server_log, 30);
            bail!("pg_ctl start 失败（端口 {}）:\n{}", self.port, log.join("\n"));
        }
        self.wait_ready(std::time::Duration::from_secs(10))
    }

    pub fn wait_ready(&self, timeout: std::time::Duration) -> anyhow::Result<()> {
        let psql = self.bin_dir.join("psql");
        // initdb 只建默认 postgres 库；就绪探测打 postgres 库，就绪后再建 e2e 库
        let dsn = format!("postgres://{PG_USER}@127.0.0.1:{}/postgres", self.port);
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let ok = Command::new(&psql)
                .arg(&dsn)
                .arg("-c").arg("SELECT 1")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if ok {
                self.create_database()?;
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                bail!("postgres 就绪探测超时（{}s）", timeout.as_secs());
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }

    /// 创建业务库（幂等：已存在则跳过）
    fn create_database(&self) -> anyhow::Result<()> {
        let psql = self.bin_dir.join("psql");
        let dsn = format!("postgres://{PG_USER}@127.0.0.1:{}/postgres", self.port);
        let out = Command::new(&psql)
            .arg(&dsn)
            .arg("-tc").arg(format!("SELECT 1 FROM pg_database WHERE datname = '{PG_DB}'"))
            .output()
            .context("查询 pg_database")?;
        let exists = String::from_utf8_lossy(&out.stdout).trim().contains('1');
        if !exists {
            let out = Command::new(&psql)
                .arg(&dsn)
                .arg("-c").arg(format!("CREATE DATABASE {PG_DB}"))
                .output()
                .context("CREATE DATABASE")?;
            if !out.status.success() {
                bail!(
                    "CREATE DATABASE {PG_DB} 失败: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
            }
        }
        Ok(())
    }

    /// postmaster pid（data_dir/postmaster.pid 首行）
    pub fn postmaster_pid(&self) -> anyhow::Result<i32> {
        let content = std::fs::read_to_string(self.data_dir.join("postmaster.pid"))?;
        let pid: i32 = content
            .lines()
            .next()
            .ok_or_else(|| anyhow::anyhow!("postmaster.pid 空"))?
            .trim()
            .parse()?;
        Ok(pid)
    }

    /// 故障注入：kill -9 postmaster（DB 宕机）
    pub fn kill_postmaster(&self) -> anyhow::Result<()> {
        let pid = self.postmaster_pid()?;
        let r = unsafe { libc::kill(pid, libc::SIGKILL) };
        if r != 0 {
            bail!("kill postmaster {pid}: {}", std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// 故障注入：SIGSTOP postmaster（DB 半死：连接挂起不拒绝）
    pub fn sigstop_postmaster(&self) -> anyhow::Result<()> {
        let pid = self.postmaster_pid()?;
        e2e_tools::proc::signal_group(pid, libc::SIGSTOP)
    }

    pub fn sigcont_postmaster(&self) -> anyhow::Result<()> {
        let pid = self.postmaster_pid()?;
        e2e_tools::proc::signal_group(pid, libc::SIGCONT)
    }

    /// 是否在运行（pid 存活且不是 STOP 态——只做存活判断）
    pub fn is_running(&self) -> bool {
        match self.postmaster_pid() {
            Ok(pid) => unsafe { libc::kill(pid, 0) == 0 },
            Err(_) => false,
        }
    }

    /// teardown：pg_ctl stop -m immediate
    pub fn stop_immediate(&self) {
        let _ = Command::new(self.bin_dir.join("pg_ctl"))
            .arg("stop")
            .arg("-D").arg(&self.data_dir)
            .arg("-m").arg("immediate")
            .arg("-t").arg("5")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }

    pub fn psql(&self, sql: &str) -> anyhow::Result<String> {
        let psql = self.bin_dir.join("psql");
        let dsn = format!("postgres://{PG_USER}@127.0.0.1:{}/{}", self.port, PG_DB);
        let out = Command::new(&psql)
            .arg(&dsn)
            .arg("-v").arg("ON_ERROR_STOP=1")
            .arg("-c").arg(sql)
            .output()
            .context("psql 执行")?;
        if !out.status.success() {
            bail!(
                "psql 失败: {}\nSQL: {sql}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    /// fixture 写入走 psql（独立于 sqlx 连接，避免与探针连接池互相干扰）
    pub fn psql_file(&self, file: &Path) -> anyhow::Result<String> {
        let psql = self.bin_dir.join("psql");
        let dsn = format!("postgres://{PG_USER}@127.0.0.1:{}/{}", self.port, PG_DB);
        let out = Command::new(&psql)
            .arg(&dsn)
            .arg("-v").arg("ON_ERROR_STOP=1")
            .arg("-f").arg(file)
            .output()
            .context("psql -f")?;
        if !out.status.success() {
            bail!(
                "psql -f 失败: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }
}

#[cfg(unix)]
fn set_mode_700(p: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

impl Drop for PostgresInstance {
    // postmaster 由 pg_ctl 守护化拉起，runner 死它不死；start_env 提前返回路径（探活/fixture 失败）的兜底
    fn drop(&mut self) {
        self.stop_immediate();
    }
}
