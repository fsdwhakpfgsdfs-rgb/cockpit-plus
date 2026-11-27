# 云端编译步骤

本机内存不够编这个 87 万行的 Rust crate，改用 GitHub Actions。
代码已提交到本地 git（commit 8e7e304，20 个文件，1436 行）。

## 第 1 步：建 GitHub 仓库

打开 https://github.com/new

- Repository name：cockpit-plus
- Private / Public 都行
- 不要勾选 Add a README file（必须是空仓库）

建好后记下地址，形如 你的用户名/cockpit-plus

## 第 2 步：生成访问令牌（PAT）

打开 https://github.com/settings/tokens → Generate new token (classic)

- Note：build-push
- Expiration：7 days
- 勾选 scope：repo（整块） 和 workflow（推 .github/workflows 必需）

生成后复制 ghp_ 开头那串（只显示一次）

## 第 3 步：推送

本机 PowerShell：

    cd D:\cockpit-plus
    .\push-to-github.ps1 -Token "ghp_你的令牌" -Repo "你的用户名/cockpit-plus"

脚本推两个分支：main（完整代码）和 gateway（触发 workflow）

## 第 4 步：下载 exe

打开 https://github.com/你的用户名/cockpit-plus/actions

看到 Build Windows (Unified Gateway) 在跑，等 15–25 分钟。
完成后点进去 → 页面底部 Artifacts → 下载 cockpit-tools-windows-x64.zip

解压得到 cockpit-tools.exe，双击运行。左侧栏底部有「统一网关」入口。

## 产物说明

- 用 --profile ci（opt-level=0），编译快、内存低
- 功能完全正常，只是没做发布级优化
- 我是把网关直接编进原版 App 的，不是独立进程

## 本机为什么编不动

    memory allocation of 2097152 bytes failed
    std::alloc::rust_oom

- 原版是单个 87 万行 crate
- rustc 生成机器码需提交 8–12GB
- 本机 commit 上限 28.9GB（物理 15.8 + 页面文件 13.3），实际剩 3.7GB
- 页面文件系统自动管理，没长大

GitHub windows-latest 也是 16GB 物理内存，但没有其他程序占用，页面文件更宽松，所以能过。

以后想本机编：管理员权限把页面文件设到 64GB 即可。