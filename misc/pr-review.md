# PR Review Guide

AtomGit 使用 GitLab 兼容协议，PR 称为 Merge Request，远程引用路径为 `refs/merge-requests/{N}/head`。

## 前置条件

添加 atomgit 上游仓库为远程（只需一次）：

```bash
git remote add atomgit https://atomgit.com/openeuler/gateway.git
```

## 查看 PR/MR

```bash
# 1. 获取指定 MR 到本地分支
git fetch atomgit refs/merge-requests/{N}/head:pr-{N}

# 2. 查看提交记录
git log --oneline pr-{N} --not atomgit/master

# 3. 查看改动文件列表
git diff --stat atomgit/master...pr-{N}

# 4. 查看完整 diff
git diff atomgit/master...pr-{N}

# 5. 查看提交作者和描述
git show pr-{N} --format="%H%n%an <%ae>%n%s%n%b" --no-patch
```

## 注意事项

- AtomGit 的 Web 界面和 API（`/api/v5/`）均需登录才能访问，无法直接通过 curl 或浏览器获取 PR 内容
- GitCode 镜像（`gitcode.com/openeuler/gateway`）同样需要登录
- GitHub 镜像不存在或未同步 PR
- 因此通过 git fetch 远程引用是唯一可靠的查看方式
